# Test certificate chain (T10.2)

A REAL three-level chain — root CA → intermediate CA → leaf — so a test can prove
the presented bundle actually VERIFIES (the older `tls.rs` case used an unrelated
self-signed cert as the "intermediate" and disabled verification, so it only
proved the bytes were carried).

Regenerate with exactly these commands (run from **this directory**: an earlier
draft of the plan wrote the artifacts into the caller's CWD instead):

```bash
mkdir -p crates/hydra-server/tests/fixtures/chain && cd crates/hydra-server/tests/fixtures/chain

openssl req -x509 -newkey rsa:2048 -nodes -keyout root.key -out root.crt \
  -days 3650 -subj "/CN=Hydra Test Root"

openssl req -newkey rsa:2048 -nodes -keyout intermediate.key -out intermediate.csr \
  -subj "/CN=Hydra Test Intermediate"
openssl x509 -req -in intermediate.csr -CA root.crt -CAkey root.key -set_serial 1 \
  -days 3650 -extfile <(printf "basicConstraints=CA:TRUE\nkeyUsage=keyCertSign") \
  -out intermediate.crt

openssl req -newkey rsa:2048 -nodes -keyout leaf.key -out leaf.csr -subj "/CN=acme.com"
openssl x509 -req -in leaf.csr -CA intermediate.crt -CAkey intermediate.key -set_serial 2 \
  -days 3650 -extfile <(printf "subjectAltName=DNS:acme.com") -out leaf.crt

rm -f intermediate.csr leaf.csr
```

Sanity check (this is what the test asserts, with real verification enabled):

```bash
# The chain verifies when the intermediate is supplied…
openssl verify -CAfile root.crt -untrusted intermediate.crt leaf.crt   # ⇒ leaf.crt: OK
# …and does NOT verify without it (the counter-proof in the test):
openssl verify -CAfile root.crt leaf.crt
#   ⇒ error 20 at 0 depth lookup: unable to get local issuer certificate
```

## Why these files are committed

The suite must not depend on `openssl` being installed on the test host, and a
chain that is generated at test time cannot be asserted against a fixed root.
Regenerating them changes every fingerprint, so regenerate only if you also
re-run the suite; nothing else references them.
