import argparse
import hashlib
import ipaddress
import json
import os
import re
import resource
import select
import signal
import struct
import subprocess
import sys
import time


BLOCK_SIZE = 4096
MAX_BLOCKS = 1536
MAX_FLOWS = 32
MAX_FRAME = 262144
MAX_SUMMARY = 3 * 1024 * 1024
MAX_SPAN = 16 * 1024 * 1024
SEQUENCE_MODULUS = 1 << 32
SCHEMA = "peer-fingerprint-v1"


class InvalidCapture(Exception):
    pass


class CaptureLimit(Exception):
    pass


class InterruptedCapture(Exception):
    pass


def read_exact(stream, length, allow_eof=False):
    result = bytearray()
    while len(result) < length:
        chunk = stream.read(length - len(result))
        if not chunk:
            if not result and allow_eof:
                return None
            raise InvalidCapture("truncated_capture")
        result.extend(chunk)
    return result


class Block:
    def __init__(self):
        self.data = bytearray(BLOCK_SIZE)
        self.coverage = bytearray(BLOCK_SIZE // 8)
        self.count = 0
        self.first_ns = None
        self.last_ns = None

    def insert(self, offset, payload, timestamp_ns):
        conflict = False
        for index, value in enumerate(payload, offset):
            byte_index, bit_index = divmod(index, 8)
            mask = 1 << bit_index
            if self.coverage[byte_index] & mask:
                conflict |= self.data[index] != value
            else:
                self.data[index] = value
                self.coverage[byte_index] |= mask
                self.count += 1
                self.first_ns = timestamp_ns if self.first_ns is None else min(self.first_ns, timestamp_ns)
                self.last_ns = timestamp_ns if self.last_ns is None else max(self.last_ns, timestamp_ns)
        return conflict


class Flow:
    def __init__(self, sequence, syn):
        self.anchor = sequence
        self.minimum = sequence
        self.maximum = sequence
        self.generation = "syn:" + str(sequence) if syn else "midstream"
        self.closed = False
        self.blocks = {}
        self.conflict = False

    def unwrap(self, sequence):
        return self.anchor + ((sequence - self.anchor + (1 << 31)) % SEQUENCE_MODULUS - (1 << 31))


class Assembler:
    def __init__(self, peers, block_limit=MAX_BLOCKS):
        self.peers = tuple(sorted(str(ipaddress.IPv4Address(peer)) for peer in peers))
        if len(self.peers) != 2 or self.peers[0] == self.peers[1]:
            raise InvalidCapture("invalid_pair")
        self.block_limit = min(block_limit, MAX_BLOCKS)
        self.block_count = 0
        self.flows = {}
        self.issues = set()
        self.packets = 0

    def segment(self, source, destination, source_port, destination_port, sequence, flags, payload, timestamp_ns):
        if tuple(sorted((source, destination))) != self.peers or 2380 not in (source_port, destination_port):
            raise InvalidCapture("out_of_scope_packet")
        self.packets += 1
        key = f"{source}:{source_port}>{destination}:{destination_port}"
        syn = bool(flags & 2)
        if key not in self.flows:
            if len(self.flows) >= MAX_FLOWS:
                raise CaptureLimit("flow_budget")
            self.flows[key] = Flow(sequence, syn)
        flow = self.flows[key]
        if syn and (flow.generation != "syn:" + str(sequence) or flow.closed):
            flow.conflict = True
            self.issues.add("connection_generation_ambiguous")
        if flow.closed and payload:
            flow.conflict = True
            self.issues.add("data_after_close")
        if not payload:
            flow.closed |= bool(flags & 5)
            return
        absolute = flow.unwrap(sequence) + int(syn)
        proposed_minimum = min(flow.minimum, absolute)
        proposed_maximum = max(flow.maximum, absolute + len(payload))
        if proposed_maximum - proposed_minimum > MAX_SPAN:
            raise CaptureLimit("sequence_span_budget")
        flow.minimum = proposed_minimum
        flow.maximum = proposed_maximum
        cursor = 0
        view = memoryview(payload)
        while cursor < len(view):
            block_index, offset = divmod(absolute + cursor, BLOCK_SIZE)
            if block_index not in flow.blocks:
                if self.block_count >= self.block_limit:
                    raise CaptureLimit("assembly_budget")
                flow.blocks[block_index] = Block()
                self.block_count += 1
            length = min(BLOCK_SIZE - offset, len(view) - cursor)
            if flow.blocks[block_index].insert(offset, view[cursor:cursor + length], timestamp_ns):
                flow.conflict = True
                self.issues.add("conflicting_overlap")
            cursor += length
        flow.closed |= bool(flags & 5)

    def summary(self, window_id, endpoint, intended_start_ns, intended_end_ns, started_ns, ended_ns):
        records = []
        incomplete = 0
        midstream = 0
        for key, flow in sorted(self.flows.items()):
            midstream += flow.generation == "midstream"
            observed = sum(block.count for block in flow.blocks.values())
            if observed and flow.maximum - flow.minimum - int(flow.generation.startswith("syn:")) > observed:
                self.issues.add("sequence_gap")
            for block_index, block in sorted(flow.blocks.items()):
                if block.count != BLOCK_SIZE:
                    incomplete += 1
                    continue
                records.append({
                    "flow": key,
                    "generation": flow.generation,
                    "seq": (block_index * BLOCK_SIZE) % SEQUENCE_MODULUS,
                    "length": BLOCK_SIZE,
                    "sha256": hashlib.sha256(block.data).hexdigest(),
                    "first_ns": block.first_ns,
                    "last_ns": block.last_ns,
                    "conflict": flow.conflict,
                })
        return {
            "schema": SCHEMA,
            "window_id": window_id,
            "endpoint": endpoint,
            "peers": list(self.peers),
            "intended_start_ns": intended_start_ns,
            "intended_end_ns": intended_end_ns,
            "started_ns": started_ns,
            "ended_ns": ended_ns,
            "packets": self.packets,
            "midstream_flows": midstream,
            "incomplete_blocks": incomplete,
            "issues": sorted(self.issues),
            "records": records,
            "attribution": "not_established",
            "required_external_evidence": ["same_window_badmac", "local_offload_review", "connection_generation_review"],
        }


def parse_frame(frame, link_type, assembler, timestamp_ns):
    view = memoryview(frame)
    if link_type == 1:
        if len(view) < 14 or bytes(view[12:14]) != b"\x08\x00":
            raise InvalidCapture("unsupported_ethernet")
        view = view[14:]
    elif link_type == 113:
        if len(view) < 16 or bytes(view[14:16]) != b"\x08\x00":
            raise InvalidCapture("unsupported_sll")
        view = view[16:]
    elif link_type == 276:
        if len(view) < 20 or bytes(view[:2]) != b"\x08\x00":
            raise InvalidCapture("unsupported_sll2")
        view = view[20:]
    elif link_type not in (101, 228):
        raise InvalidCapture("unsupported_link_type")
    if len(view) < 20 or view[0] >> 4 != 4:
        raise InvalidCapture("invalid_ipv4")
    header_length = (view[0] & 15) * 4
    total_length = int.from_bytes(view[2:4], "big")
    if header_length < 20 or total_length < header_length + 20 or total_length > len(view):
        raise InvalidCapture("truncated_or_offload_ambiguous_ipv4")
    if len(view) > max(total_length, 46):
        raise InvalidCapture("offload_ambiguous_length")
    if int.from_bytes(view[6:8], "big") & 0x3FFF or view[9] != 6:
        raise InvalidCapture("fragment_or_non_tcp")
    source = str(ipaddress.IPv4Address(bytes(view[12:16])))
    destination = str(ipaddress.IPv4Address(bytes(view[16:20])))
    tcp = view[header_length:total_length]
    tcp_length = (tcp[12] >> 4) * 4
    if tcp_length < 20 or tcp_length > len(tcp):
        raise InvalidCapture("invalid_tcp_header")
    source_port, destination_port, sequence = struct.unpack_from("!HHI", tcp)
    assembler.segment(source, destination, source_port, destination_port, sequence, tcp[13], tcp[tcp_length:], timestamp_ns)


def read_pcap(stream, assembler):
    header = read_exact(stream, 24)
    formats = {
        b"\xd4\xc3\xb2\xa1": ("<", 1000),
        b"\xa1\xb2\xc3\xd4": (">", 1000),
        b"\x4d\x3c\xb2\xa1": ("<", 1),
        b"\xa1\xb2\x3c\x4d": (">", 1),
    }
    if bytes(header[:4]) not in formats:
        raise InvalidCapture("unsupported_capture_format")
    endian, scale = formats[bytes(header[:4])]
    major, minor, _, _, snap_length, link_type = struct.unpack(endian + "HHIIII", header[4:])
    if (major, minor) != (2, 4) or not 64 <= snap_length <= MAX_FRAME:
        raise InvalidCapture("invalid_capture_header")
    if link_type not in (1, 101, 113, 228, 276):
        raise InvalidCapture("unsupported_link_type")
    while True:
        record = read_exact(stream, 16, allow_eof=True)
        if record is None:
            return
        seconds, fraction, included, original = struct.unpack(endian + "IIII", record)
        if included > snap_length or included > MAX_FRAME or included != original:
            raise InvalidCapture("truncated_or_oversized_record")
        if fraction * scale >= 1_000_000_000:
            raise InvalidCapture("invalid_capture_timestamp")
        frame = read_exact(stream, included)
        parse_frame(frame, link_type, assembler, seconds * 1_000_000_000 + fraction * scale)


def signal_group(process_group, signum):
    try:
        os.killpg(process_group, signum)
    except ProcessLookupError:
        pass


def watchdog(process_group, control, deadline):
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    try:
        remaining = max(0, deadline - time.monotonic())
        readable, _, _ = select.select([control], [], [], remaining)
        if not readable:
            signal_group(process_group, signal.SIGINT)
            select.select([control], [], [], 2)
        signal_group(process_group, signal.SIGKILL)
    finally:
        os.close(control)
        os._exit(0)


def supervise(command, consumer, duration):
    if not 0 < duration <= 240:
        raise InvalidCapture("invalid_duration")
    capture = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0, start_new_session=True)
    control_read, control_write = os.pipe()
    try:
        guard = os.fork()
    except BaseException:
        signal_group(capture.pid, signal.SIGKILL)
        capture.wait()
        os.close(control_read)
        os.close(control_write)
        raise
    if guard == 0:
        os.close(control_write)
        capture.stdout.close()
        capture.stderr.close()
        watchdog(capture.pid, control_read, time.monotonic() + duration)
    os.close(control_read)
    result = None
    failure = None
    try:
        result = consumer(capture.stdout)
    except BaseException as error:
        failure = error
    finally:
        previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
        signal_group(capture.pid, signal.SIGINT)
        try:
            capture.wait(timeout=1)
        except subprocess.TimeoutExpired:
            signal_group(capture.pid, signal.SIGKILL)
            capture.wait(timeout=2)
        finally:
            os.close(control_write)
            os.waitpid(guard, 0)
            capture.stdout.close()
        diagnostic = capture.stderr.read(32769)
        capture.stderr.close()
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
    if failure is not None:
        failure.capture_returncode = capture.returncode
        failure.capture_startup_rejected = any(
            marker in diagnostic for marker in (
                b"You don't have permission", b"Operation not permitted", b"Permission denied",
            )
        )
        raise failure
    return result, capture.returncode, diagnostic


def capture_command(peers, interface="tailscale0"):
    first, second = (str(ipaddress.IPv4Address(peer)) for peer in peers)
    expression = f"tcp port 2380 and ((src host {first} and dst host {second}) or (src host {second} and dst host {first}))"
    return ["/usr/bin/tcpdump", "-i", interface, "-p", "-nn", "-s", "0", "-B", "512", "-U", "-w", "-", expression]


def validate_capabilities(status):
    fields = dict(line.split(":", 1) for line in status.splitlines() if ":" in line)
    net_raw = 1 << 13
    for field in ("CapEff", "CapPrm", "CapBnd"):
        if int(fields.get(field, "0"), 16) != net_raw:
            raise InvalidCapture("net_raw_only_required")
    for field in ("CapInh", "CapAmb"):
        if int(fields.get(field, "0"), 16) & ~net_raw:
            raise InvalidCapture("net_raw_only_required")
    if fields.get("NoNewPrivs", "").strip() != "1":
        raise InvalidCapture("no_new_privileges_required")


def capture_statistics(diagnostic):
    if len(diagnostic) > 32768:
        raise InvalidCapture("capture_diagnostics_limit")
    result = {}
    for label, pattern in {
        "captured": rb"(?m)^(\d+) packets? captured$",
        "received": rb"(?m)^(\d+) packets? received by filter$",
        "dropped": rb"(?m)^(\d+) packets? dropped by kernel$",
    }.items():
        matches = re.findall(pattern, diagnostic)
        if len(matches) != 1:
            raise InvalidCapture("capture_statistics_missing")
        result[label] = int(matches[0])
    return result


def safe_emit(summary, stream=sys.stdout):
    encoded = json.dumps(summary, separators=(",", ":"), sort_keys=True)
    if len(encoded.encode("utf-8")) + 1 > MAX_SUMMARY:
        encoded = '{"schema":"peer-fingerprint-v1","issues":["summary_budget"],"attribution":"not_established"}'
    stream.write(encoded + "\n")
    stream.flush()


def run_capture(arguments):
    with open("/proc/self/status") as status:
        validate_capabilities(status.read())
    if not re.fullmatch("[0-9a-f]{32}", arguments.window_id):
        raise InvalidCapture("invalid_window_id")
    if arguments.endpoint not in ("left", "right") or not 1 <= arguments.duration <= 240:
        raise InvalidCapture("invalid_capture_parameters")
    start = arguments.start_unix_ns
    wait_seconds = (start - time.time_ns()) / 1_000_000_000
    if not 0 <= wait_seconds <= 30:
        raise InvalidCapture("late_or_invalid_start")
    assembler = Assembler((arguments.peer_a, arguments.peer_b))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    time.sleep(wait_seconds)
    started = time.time_ns()
    intended_end = start + arguments.duration * 1_000_000_000
    remaining = (intended_end - started) / 1_000_000_000
    if remaining <= 0:
        raise InvalidCapture("missed_window")
    statistics = None
    returncode = None
    try:
        _, returncode, diagnostic = supervise(
            capture_command(assembler.peers),
            lambda stream: read_pcap(stream, assembler),
            remaining,
        )
        statistics = capture_statistics(diagnostic)
        if statistics["dropped"]:
            assembler.issues.add("capture_loss")
        if statistics["captured"] != assembler.packets:
            assembler.issues.add("capture_count_mismatch")
        if returncode != 0:
            assembler.issues.add("capture_exit")
    except (InvalidCapture, CaptureLimit) as error:
        assembler.issues.add(str(error))
    except InterruptedCapture:
        assembler.issues.add("interrupted")
    ended = time.time_ns()
    if ended < intended_end:
        assembler.issues.add("early_capture_end")
    summary = assembler.summary(arguments.window_id, arguments.endpoint, start, intended_end, started, ended)
    summary["capture_statistics"] = statistics
    summary["capture_returncode"] = returncode
    safe_emit(summary)
    return 2 if summary["issues"] else 0


def validate_summary(summary):
    if summary.get("schema") != SCHEMA or summary.get("attribution") != "not_established":
        raise InvalidCapture("invalid_summary")
    for field in ("intended_start_ns", "intended_end_ns", "started_ns", "ended_ns"):
        if type(summary.get(field)) is not int:
            raise InvalidCapture("invalid_summary_time")
    if not summary["started_ns"] < summary["ended_ns"] or not summary["intended_start_ns"] < summary["intended_end_ns"]:
        raise InvalidCapture("invalid_summary_time")
    if not isinstance(summary.get("issues"), list) or not isinstance(summary.get("records"), list):
        raise InvalidCapture("invalid_summary")
    if len(summary["records"]) > MAX_BLOCKS:
        raise InvalidCapture("invalid_summary_budget")
    seen = set()
    for record in summary["records"]:
        if type(record.get("seq")) is not int or not 0 <= record["seq"] < SEQUENCE_MODULUS or record["seq"] % BLOCK_SIZE:
            raise InvalidCapture("invalid_summary_interval")
        if record.get("length") != BLOCK_SIZE or not re.fullmatch("[0-9a-f]{64}", record.get("sha256", "")):
            raise InvalidCapture("invalid_summary_interval")
        if type(record.get("first_ns")) is not int or type(record.get("last_ns")) is not int or record["first_ns"] > record["last_ns"]:
            raise InvalidCapture("invalid_summary_interval")
        key = (record["flow"], record["generation"], record["seq"], record["length"])
        if key in seen:
            raise InvalidCapture("duplicate_summary_interval")
        seen.add(key)


def compare_summaries(left, right):
    validate_summary(left)
    validate_summary(right)
    reasons = {"same_window_badmac_review_required", "local_offload_review_required", "connection_generation_review_required"}
    compatible = True
    for field in ("window_id", "peers", "intended_start_ns", "intended_end_ns"):
        if left.get(field) != right.get(field):
            compatible = False
            reasons.add("window_or_pair_mismatch")
    if {left.get("endpoint"), right.get("endpoint")} != {"left", "right"}:
        compatible = False
        reasons.add("endpoint_mismatch")
    overlap_start = max(left["started_ns"], right["started_ns"])
    overlap_end = min(left["ended_ns"], right["ended_ns"])
    if overlap_start >= overlap_end:
        compatible = False
        reasons.add("no_common_window")
    for summary in (left, right):
        if summary["issues"]:
            compatible = False
            reasons.add("capture_quality_failure")
        statistics = summary.get("capture_statistics")
        if not statistics or statistics.get("dropped") != 0 or summary.get("capture_returncode") != 0:
            compatible = False
            reasons.add("capture_statistics_invalid")
    matched = equal = unequal = 0
    if compatible:
        def intervals(summary):
            return {
                (record["flow"], record["generation"], record["seq"], record["length"]): record["sha256"]
                for record in summary["records"]
                if not record.get("conflict", True)
                and overlap_start <= record["first_ns"] <= record["last_ns"] <= overlap_end
            }
        left_intervals = intervals(left)
        right_intervals = intervals(right)
        for key in left_intervals.keys() & right_intervals.keys():
            matched += 1
            equal += left_intervals[key] == right_intervals[key]
            unequal += left_intervals[key] != right_intervals[key]
    if not matched:
        reasons.add("no_comparable_intervals")
    return {
        "schema": SCHEMA,
        "verdict": "inconclusive",
        "attribution": "not_established",
        "matched_intervals": matched,
        "equal_intervals": equal,
        "unequal_intervals": unequal,
        "required_review": sorted(reasons),
    }


def load_summary(path):
    with open(path, "rb") as source:
        content = source.read(MAX_SUMMARY + 1)
    if len(content) > MAX_SUMMARY:
        raise InvalidCapture("summary_budget")
    return json.loads(content)


def interrupt_capture(signum, frame):
    raise InterruptedCapture()


def main():
    parser = argparse.ArgumentParser(description="Bounded deployment diagnostic support; never certifies PASS or corruption.")
    commands = parser.add_subparsers(dest="operation", required=True)
    capture = commands.add_parser("capture")
    capture.add_argument("--peer-a", required=True)
    capture.add_argument("--peer-b", required=True)
    capture.add_argument("--window-id", required=True)
    capture.add_argument("--endpoint", required=True)
    capture.add_argument("--start-unix-ns", required=True, type=int)
    capture.add_argument("--duration", type=int, default=240)
    compare = commands.add_parser("compare")
    compare.add_argument("left")
    compare.add_argument("right")
    arguments = parser.parse_args()
    signal.signal(signal.SIGTERM, interrupt_capture)
    signal.signal(signal.SIGINT, interrupt_capture)
    try:
        if arguments.operation == "capture":
            return run_capture(arguments)
        safe_emit(compare_summaries(load_summary(arguments.left), load_summary(arguments.right)))
        return 0
    except BaseException as error:
        if isinstance(error, (KeyboardInterrupt, InterruptedCapture)):
            issue = "interrupted"
        elif isinstance(error, (InvalidCapture, CaptureLimit)):
            issue = str(error)
        else:
            issue = "diagnostic_failure"
        try:
            safe_emit({"schema": SCHEMA, "issues": [issue], "attribution": "not_established"})
        except BaseException:
            pass
        return 2


if __name__ == "__main__":
    sys.exit(main())
