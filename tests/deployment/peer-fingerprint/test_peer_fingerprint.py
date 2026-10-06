import copy
import io
import ipaddress
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import time
import unittest

import peer_fingerprint as helper


PEERS = ("192.0.2.1", "192.0.2.2")
PAYLOAD = bytes(range(256)) * 16
START = 1_000_000_000
END = 10_000_000_000


def add(assembler, sequence, payload, flags=16, timestamp_ns=2_000_000_000):
    assembler.segment(PEERS[0], PEERS[1], 30000, 2380, sequence, flags, payload, timestamp_ns)


def evidence(assembler, endpoint="left"):
    result = assembler.summary("0" * 32, endpoint, START, END, START, END)
    result["capture_statistics"] = {"captured": assembler.packets, "received": assembler.packets, "dropped": 0}
    result["capture_returncode"] = 0
    return result


def packet(sequence, payload=PAYLOAD, link_type=101, flags=16, source=PEERS[0], destination=PEERS[1], destination_port=2380):
    ipv4 = bytearray(20)
    ipv4[0] = 0x45
    struct.pack_into("!H", ipv4, 2, 40 + len(payload))
    ipv4[8:10] = bytes((64, 6))
    ipv4[12:16] = ipaddress.IPv4Address(source).packed
    ipv4[16:20] = ipaddress.IPv4Address(destination).packed
    tcp = struct.pack("!HHIIBBHHH", 30000, destination_port, sequence, 0, 0x50, flags, 65535, 0, 0)
    prefixes = {
        1: bytes(12) + b"\x08\x00",
        101: b"",
        228: b"",
        113: bytes(14) + b"\x08\x00",
        276: b"\x08\x00" + bytes(18),
    }
    return prefixes[link_type] + ipv4 + tcp + payload


def pcap(frames, link_type=101, endian="<", nanos=False):
    magic = 0xA1B23C4D if nanos else 0xA1B2C3D4
    result = bytearray(struct.pack(endian + "IHHIIII", magic, 2, 4, 0, 0, helper.MAX_FRAME, link_type))
    for frame in frames:
        result.extend(struct.pack(endian + "IIII", 2, 0, len(frame), len(frame)))
        result.extend(frame)
    return result


def process_active(process_id):
    try:
        status = Path(f"/proc/{process_id}/stat").read_text()
    except FileNotFoundError:
        return False
    return status.rsplit(")", 1)[1].split()[0] not in ("Z", "X")


def wait_inactive(process_id, seconds=4):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if not process_active(process_id):
            return True
        time.sleep(0.02)
    return not process_active(process_id)


class ReassemblyTests(unittest.TestCase):
    def test_retransmission_segmentation_and_reordering(self):
        left = helper.Assembler(PEERS)
        right = helper.Assembler(PEERS)
        add(left, 4096, PAYLOAD)
        add(left, 4096, PAYLOAD)
        for offset in reversed(range(0, len(PAYLOAD), 256)):
            add(right, 4096 + offset, PAYLOAD[offset:offset + 256])
        add(right, 4100, PAYLOAD[4:700])
        result = helper.compare_summaries(evidence(left), evidence(right, "right"))
        self.assertEqual(result["equal_intervals"], 1)
        self.assertEqual(result["unequal_intervals"], 0)
        self.assertEqual(result["verdict"], "inconclusive")

    def test_synthetic_change_is_not_automatic_corruption_verdict(self):
        left = helper.Assembler(PEERS)
        right = helper.Assembler(PEERS)
        add(left, 4096, PAYLOAD)
        add(right, 4096, b"fixture-only" + PAYLOAD[12:])
        result = helper.compare_summaries(evidence(left), evidence(right, "right"))
        self.assertEqual(result["unequal_intervals"], 1)
        self.assertEqual(result["attribution"], "not_established")
        self.assertIn("local_offload_review_required", result["required_review"])

    def test_conflicting_partial_retransmission_invalidates_completed_block(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4096, PAYLOAD)
        add(assembler, 4097, b"x")
        summary = evidence(assembler)
        self.assertIn("conflicting_overlap", summary["issues"])
        self.assertTrue(summary["records"][0]["conflict"])
        other = copy.deepcopy(summary)
        other["endpoint"] = "right"
        self.assertEqual(helper.compare_summaries(summary, other)["matched_intervals"], 0)

    def test_gap_is_not_silently_zero_filled(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4096, PAYLOAD[:100])
        add(assembler, 4300, PAYLOAD[204:])
        summary = evidence(assembler)
        self.assertIn("sequence_gap", summary["issues"])
        self.assertEqual(summary["records"], [])

    def test_partial_edges_do_not_invent_bytes(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4196, PAYLOAD[:100])
        summary = evidence(assembler)
        self.assertEqual(summary["records"], [])
        self.assertEqual(summary["incomplete_blocks"], 1)
        self.assertNotIn("sequence_gap", summary["issues"])

    def test_absolute_sequence_wrap_and_different_capture_start(self):
        left = helper.Assembler(PEERS)
        right = helper.Assembler(PEERS)
        add(left, (1 << 32) - 4096, PAYLOAD)
        add(left, 0, PAYLOAD)
        add(right, 0, PAYLOAD)
        result = helper.compare_summaries(evidence(left), evidence(right, "right"))
        self.assertEqual(result["equal_intervals"], 1)

    def test_syn_consumes_one_sequence_number(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4095, b"", flags=2)
        add(assembler, 4096, PAYLOAD)
        summary = evidence(assembler)
        self.assertEqual(summary["records"][0]["seq"], 4096)
        self.assertEqual(summary["records"][0]["generation"], "syn:4095")
        self.assertNotIn("sequence_gap", summary["issues"])

    def test_syn_with_data_and_fin_sequence_handling(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4095, PAYLOAD, flags=2)
        add(assembler, 8192, b"", flags=17)
        summary = evidence(assembler)
        self.assertEqual(len(summary["records"]), 1)
        self.assertEqual(summary["issues"], [])

    def test_tuple_reuse_is_ambiguous(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4096, PAYLOAD)
        add(assembler, 8192, b"", flags=4)
        add(assembler, 20000, b"", flags=2)
        self.assertIn("connection_generation_ambiguous", evidence(assembler)["issues"])

    def test_bounds_are_explicit(self):
        assembler = helper.Assembler(PEERS, block_limit=1)
        add(assembler, 4096, PAYLOAD)
        with self.assertRaisesRegex(helper.CaptureLimit, "assembly_budget"):
            add(assembler, 8192, PAYLOAD)
        with self.assertRaisesRegex(helper.CaptureLimit, "sequence_span_budget"):
            add(assembler, 1 << 30, PAYLOAD)

    def test_summary_never_contains_payload(self):
        assembler = helper.Assembler(PEERS)
        marker = b"fixture-only-do-not-persist-raw-payload"
        add(assembler, 4096, (marker * 200)[:4096])
        output = io.StringIO()
        helper.safe_emit(evidence(assembler), output)
        self.assertNotIn(marker.decode(), output.getvalue())
        self.assertLess(len(output.getvalue()), helper.MAX_SUMMARY)

    def test_flow_budget(self):
        assembler = helper.Assembler(PEERS)
        for offset in range(helper.MAX_FLOWS):
            assembler.segment(*PEERS, 30000 + offset, 2380, 4096, 16, b"x", 2_000_000_000)
        with self.assertRaisesRegex(helper.CaptureLimit, "flow_budget"):
            assembler.segment(*PEERS, 31000, 2380, 4096, 16, b"x", 2_000_000_000)


class PcapTests(unittest.TestCase):
    def test_supported_link_types_endianness_and_time_units(self):
        for link_type in (1, 101, 113, 228, 276):
            for endian in ("<", ">"):
                for nanos in (False, True):
                    with self.subTest(link_type=link_type, endian=endian, nanos=nanos):
                        assembler = helper.Assembler(PEERS)
                        helper.read_pcap(io.BytesIO(pcap([packet(4096, link_type=link_type)], link_type, endian, nanos)), assembler)
                        self.assertEqual(len(evidence(assembler)["records"]), 1)

    def test_variable_ip_and_tcp_headers(self):
        frame = bytearray(packet(4096))
        frame[0] = 0x46
        frame[20:20] = bytes(4)
        frame[36] = 0x60
        frame[44:44] = bytes(4)
        struct.pack_into("!H", frame, 2, len(frame))
        assembler = helper.Assembler(PEERS)
        helper.read_pcap(io.BytesIO(pcap([frame])), assembler)
        self.assertEqual(len(evidence(assembler)["records"]), 1)

    def test_large_segmentation_representation(self):
        left = helper.Assembler(PEERS)
        right = helper.Assembler(PEERS)
        helper.read_pcap(io.BytesIO(pcap([packet(4096, PAYLOAD * 8)])), left)
        frames = [packet(4096 + offset, (PAYLOAD * 8)[offset:offset + 512]) for offset in range(0, len(PAYLOAD) * 8, 512)]
        helper.read_pcap(io.BytesIO(pcap(frames)), right)
        self.assertEqual(helper.compare_summaries(evidence(left), evidence(right, "right"))["equal_intervals"], 8)

    def test_wrong_pair_or_port_rejected(self):
        for frame in (packet(4096, source="192.0.2.99"), packet(4096, destination_port=2379)):
            with self.assertRaisesRegex(helper.InvalidCapture, "out_of_scope_packet"):
                helper.read_pcap(io.BytesIO(pcap([frame])), helper.Assembler(PEERS))

    def test_truncation_fragments_and_oversized_records(self):
        malformed = [pcap([packet(4096)])[:-1], b"fixture-only"]
        fragmented = bytearray(packet(4096))
        fragmented[6] = 0x20
        malformed.append(pcap([fragmented]))
        oversized = pcap([])
        oversized.extend(struct.pack("<IIII", 2, 0, helper.MAX_FRAME + 1, helper.MAX_FRAME + 1))
        malformed.append(oversized)
        for source in malformed:
            with self.assertRaises(helper.InvalidCapture):
                helper.read_pcap(io.BytesIO(source), helper.Assembler(PEERS))

    def test_offload_ambiguous_length_rejected(self):
        with self.assertRaisesRegex(helper.InvalidCapture, "offload_ambiguous_length"):
            helper.parse_frame(packet(4096) + b"extra", 101, helper.Assembler(PEERS), 2_000_000_000)

    def test_partial_stream_reads(self):
        class ShortReads(io.BytesIO):
            def read(self, size=-1):
                return super().read(min(size, 7))
        assembler = helper.Assembler(PEERS)
        helper.read_pcap(ShortReads(pcap([packet(4096)])), assembler)
        self.assertEqual(len(evidence(assembler)["records"]), 1)


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        assembler = helper.Assembler(PEERS)
        add(assembler, 4096, PAYLOAD)
        self.left = evidence(assembler)
        self.right = evidence(assembler, "right")

    def test_loss_missing_statistics_and_abnormal_exit_are_not_passes(self):
        for change in ({"issues": ["capture_loss"]}, {"capture_statistics": None}, {"capture_returncode": -9}):
            right = {**self.right, **change}
            result = helper.compare_summaries(self.left, right)
            self.assertEqual(result["matched_intervals"], 0)
            self.assertEqual(result["verdict"], "inconclusive")

    def test_different_window_and_nonoverlap(self):
        for change in ({"window_id": "1" * 32}, {"intended_start_ns": START + 1}, {"started_ns": END, "ended_ns": END + 100}):
            result = helper.compare_summaries(self.left, {**self.right, **change})
            self.assertEqual(result["matched_intervals"], 0)

    def test_intervals_must_be_entirely_in_common_window(self):
        self.right["started_ns"] = 3_000_000_000
        result = helper.compare_summaries(self.left, self.right)
        self.assertEqual(result["matched_intervals"], 0)

    def test_offsets_lengths_and_generations_are_not_interchangeable(self):
        self.right["records"][0]["seq"] += helper.BLOCK_SIZE
        self.assertEqual(helper.compare_summaries(self.left, self.right)["matched_intervals"], 0)
        self.right["records"][0]["length"] -= 1
        with self.assertRaises(helper.InvalidCapture):
            helper.compare_summaries(self.left, self.right)

    def test_duplicate_records_rejected(self):
        self.right["records"].append(self.right["records"][0])
        with self.assertRaisesRegex(helper.InvalidCapture, "duplicate_summary_interval"):
            helper.compare_summaries(self.left, self.right)

    def test_no_badmac_evidence_never_becomes_pass(self):
        result = helper.compare_summaries(self.left, self.right)
        self.assertEqual(result["equal_intervals"], 1)
        self.assertEqual(result["verdict"], "inconclusive")
        self.assertIn("same_window_badmac_review_required", result["required_review"])


@unittest.skipUnless(sys.platform == "linux", "Linux process-group supervision")
class CleanupTests(unittest.TestCase):
    def test_deadline_kills_capture_and_grandchild_ignoring_interrupt(self):
        source = (
            "import os,signal,subprocess,sys,time; "
            "signal.signal(signal.SIGINT,signal.SIG_IGN); "
            "child=subprocess.Popen([sys.executable,'-c','import signal,time; signal.signal(signal.SIGINT,signal.SIG_IGN); time.sleep(60)']); "
            "print(os.getpid(),child.pid,flush=True); time.sleep(60)"
        )
        captured = []
        def consume(stream):
            captured.extend(int(value) for value in stream.readline().split())
            stream.read()
        started = time.monotonic()
        helper.supervise([sys.executable, "-c", source], consume, 0.2)
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(len(captured), 2)
        self.assertTrue(all(wait_inactive(process_id) for process_id in captured))

    def test_parser_failure_reaps_capture(self):
        captured = []
        def fail(stream):
            captured.append(int(stream.readline()))
            raise helper.InvalidCapture("fixture_parser_failure")
        with self.assertRaisesRegex(helper.InvalidCapture, "fixture_parser_failure"):
            helper.supervise([sys.executable, "-c", "import os,time; print(os.getpid(),flush=True); time.sleep(60)"], fail, 2)
        self.assertTrue(wait_inactive(captured[0]))

    def test_watchdog_stops_capture_while_parser_is_stalled(self):
        captured = []
        def stall(stream):
            captured.append(int(stream.readline()))
            time.sleep(0.5)
            self.assertFalse(process_active(captured[0]))
        helper.supervise([sys.executable, "-c", "import os,time; print(os.getpid(),flush=True); time.sleep(60)"], stall, 0.1)

    def test_parent_death_closes_control_pipe_and_cleans_children(self):
        worker_source = (
            "import json,os,pathlib,sys; import peer_fingerprint as helper\n"
            "def consume(stream):\n"
            " stream.readline()\n"
            " children=pathlib.Path('/proc/self/task/'+str(os.getpid())+'/children').read_text().split()\n"
            " print(json.dumps([int(value) for value in children]),flush=True)\n"
            " stream.read()\n"
            "helper.supervise([sys.executable,'-c','import os,time; print(os.getpid(),flush=True); time.sleep(60)'],consume,5)\n"
        )
        worker = subprocess.Popen([sys.executable, "-B", "-c", worker_source], cwd=Path(__file__).parent, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            children = json.loads(worker.stdout.readline())
            self.assertEqual(len(children), 2)
            worker.kill()
            worker.wait(timeout=3)
            self.assertTrue(all(wait_inactive(process_id) for process_id in children))
        finally:
            if worker.poll() is None:
                worker.kill()
                worker.wait(timeout=3)
            worker.stdout.close()

    def test_missing_tool_does_not_leave_watchdog(self):
        with self.assertRaises(FileNotFoundError):
            helper.supervise(["/fixture-only/no-such-executable"], lambda stream: stream.read(), 1)


class BoundaryTests(unittest.TestCase):
    def test_command_is_passive_pair_only_and_has_no_raw_output_file(self):
        command = helper.capture_command(PEERS)
        self.assertEqual(command[0], "/usr/bin/tcpdump")
        self.assertIn("-p", command)
        self.assertIn("-nn", command)
        self.assertEqual(command[command.index("-w") + 1], "-")
        self.assertEqual(command[-1], "tcp port 2380 and ((src host 192.0.2.1 and dst host 192.0.2.2) or (src host 192.0.2.2 and dst host 192.0.2.1))")

    def test_statistics_only_export_counts(self):
        diagnostic = b"tcpdump: listening on fixture-only\n4 packets captured\n4 packets received by filter\n0 packets dropped by kernel\n"
        self.assertEqual(helper.capture_statistics(diagnostic), {"captured": 4, "received": 4, "dropped": 0})
        with self.assertRaises(helper.InvalidCapture):
            helper.capture_statistics(b"fixture-only missing counters")

    def test_summary_limit_fails_closed(self):
        output = io.StringIO()
        helper.safe_emit({"fixture": "x" * helper.MAX_SUMMARY}, output)
        result = json.loads(output.getvalue())
        self.assertEqual(result["issues"], ["summary_budget"])
        self.assertNotIn("fixture", result)


if __name__ == "__main__":
    unittest.main()
