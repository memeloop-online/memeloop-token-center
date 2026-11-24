"""Real S3 conditional DELETE contract, restricted to one synthetic fixture key."""
import datetime
import hashlib
import hmac
import os
import urllib.error
import urllib.parse
import urllib.request
import uuid

endpoint = urllib.parse.urlsplit(os.environ['MTC_RUSTFS_TEST_ENDPOINT'])
bucket = os.environ['MTC_RUSTFS_TEST_BUCKET']
access = os.environ['MTC_RUSTFS_TEST_ACCESS_KEY']
secret = os.environ['MTC_RUSTFS_TEST_SECRET_KEY']
key = 'conditional-delete-contract/' + str(uuid.uuid4())
uri = urllib.parse.quote('/' + bucket + '/' + key, safe='/-_.~')
body = b'synthetic conditional-delete fixture; no customer content'


def mac(key, value):
    return hmac.new(key, value.encode(), hashlib.sha256).digest()


def request(method, data=b'', etag=None):
    now = datetime.datetime.now(datetime.timezone.utc)
    day, stamp = now.strftime('%Y%m%d'), now.strftime('%Y%m%dT%H%M%SZ')
    digest = hashlib.sha256(data).hexdigest()
    headers = {'host': endpoint.netloc, 'x-amz-content-sha256': digest,
               'x-amz-date': stamp}
    if etag is not None:
        headers['if-match'] = etag
    signed = ';'.join(sorted(headers))
    canonical_headers = ''.join(name + ':' + headers[name] + '\n' for name in sorted(headers))
    canonical = '\n'.join([method, uri, '', canonical_headers, signed, digest])
    scope = day + '/us-east-1/s3/aws4_request'
    to_sign = '\n'.join(['AWS4-HMAC-SHA256', stamp, scope,
                         hashlib.sha256(canonical.encode()).hexdigest()])
    signing_key = mac(mac(mac(mac(('AWS4' + secret).encode(), day), 'us-east-1'), 's3'), 'aws4_request')
    signature = hmac.new(signing_key, to_sign.encode(), hashlib.sha256).hexdigest()
    headers['authorization'] = ('AWS4-HMAC-SHA256 Credential=' + access + '/' + scope
                                + ', SignedHeaders=' + signed + ', Signature=' + signature)
    req = urllib.request.Request(endpoint.scheme + '://' + endpoint.netloc + uri,
                                 method=method, data=data if method == 'PUT' else None,
                                 headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as response:
            return response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers, error.read()


assert request('PUT', body)[0] in (200, 201)
status, headers, _ = request('HEAD')
assert status == 200
etag = headers.get('ETag')
assert etag
wrong_status, _, _ = request('DELETE', etag='"deliberately-wrong-contract-etag"')
assert wrong_status == 412, 'server must reject a mismatched ETag'
status, _, retained = request('GET')
assert status == 200 and retained == body, 'failed precondition must retain exact bytes'
assert request('DELETE', etag=etag)[0] in (200, 204)
assert request('HEAD')[0] == 404
print('conditional DELETE: wrong ETag rejected; bytes retained; exact ETag deleted synthetic fixture')
