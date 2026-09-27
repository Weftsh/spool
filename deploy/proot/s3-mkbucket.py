#!/usr/bin/env python3
"""Create a bucket with one SigV4-signed PUT: s3-mkbucket.py ENDPOINT BUCKET KEY SECRET"""
import datetime, hashlib, hmac, sys, urllib.parse, urllib.request, urllib.error
endpoint, bucket, key, secret = sys.argv[1:5]
u = urllib.parse.urlparse(endpoint); host = u.netloc; region = "us-east-1"
now = datetime.datetime.now(datetime.timezone.utc); amz = now.strftime("%Y%m%dT%H%M%SZ"); day = now.strftime("%Y%m%d")
payload = hashlib.sha256(b"").hexdigest()
headers = {"host": host, "x-amz-content-sha256": payload, "x-amz-date": amz}
signed = ";".join(sorted(headers))
canon = "\n".join(["PUT", f"/{bucket}", "", *(f"{k}:{v}" for k, v in sorted(headers.items())), "", signed, payload])
scope = f"{day}/{region}/s3/aws4_request"
sts = "\n".join(["AWS4-HMAC-SHA256", amz, scope, hashlib.sha256(canon.encode()).hexdigest()])
k = ("AWS4" + secret).encode()
for part in (day, region, "s3", "aws4_request"):
    k = hmac.new(k, part.encode(), hashlib.sha256).digest()
sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
headers["Authorization"] = f"AWS4-HMAC-SHA256 Credential={key}/{scope}, SignedHeaders={signed}, Signature={sig}"
req = urllib.request.Request(f"{endpoint}/{bucket}", method="PUT", headers=headers)
try:
    with urllib.request.urlopen(req, timeout=30) as r:
        print(f"bucket {bucket}: created ({r.status})")
except urllib.error.HTTPError as e:
    body = e.read().decode(errors="replace")
    if "BucketAlreadyOwnedByYou" in body or "BucketAlreadyExists" in body:
        print(f"bucket {bucket}: already there")
    else:
        sys.exit(f"bucket {bucket}: {e.code} {body[:200]}")
