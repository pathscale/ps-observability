# Changelog

## blitz-control-protocol 0.7.0

- Advertise inspection protocol 2 and serialize command fields as camelCase; continue accepting the protocol 1 snake_case field names on input.
- Reject mismatched descriptor versions with both versions and matching-client guidance.
- Add DiagnosticsRequest::Evaluate and DebugResponse::Evaluated for a host to return a page script's JSON completion value.
- Document host-supplied per-stage RevisionSet counters; the shared document snapshot still reports zero for counters it does not read.
- Box the private connection event without changing the public response payload types.

## ps-qa 0.9.0

- Require a protocol 2 inspector through blitz-control-protocol ^0.7.0; protocol 1 descriptors are rejected.
- Add ps-qa eval to print a page script's JSON completion value.
- Report an expired outcome deadline as OutcomeTimeout before another inspector read, instead of poisoning the connection with a zero request timeout.
