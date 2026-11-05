# TODO

## Unverified security follow-ups

- Recheck whether the Linux agent launch trusts the shared `/tmp/tauri-blitz-agent` directory without verifying owner, group permissions, symlink resolution and cleanup; the 2026-10-14 static note did not revalidate current source or execute an exploit. If the issue remains, use an owner-controlled private directory and record the launch contract.
- Recheck whether the browser under test shares stdout with page-controlled output before trusting a host descriptor parsed from stdout; the old note was static only. If that path remains, use a separate bounded descriptor channel or validate its framing and origin.
- Recheck the secret scrubber's base64 handling when secrets occur with surrounding encoded bytes; the old static note described a match only at offset zero and did not establish an end-to-end disclosure.
- Recheck release workflow credential exposure during dependency compilation and mutable action/cache inputs. Coordinate the package-publishing boundary with ps-blitz's release owner; this old cross-repository note is unverified on current source.

These are leads from `SECURITY-AUDIT-2026-10-14.md`, which was removed after its open claims were folded here. No runtime acceptance is claimed.
