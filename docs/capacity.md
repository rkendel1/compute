# Capacity and durable reservations

Compute remote providers schedule durable jobs against normalized CPU,
memory, disk, and concurrency capacity. Available capacity is always derived
from the configured provider capacity minus active durable reservations; it
is not an independently mutated counter.

The lifecycle is:

```text
queued -> waiting_for_capacity -> reserved -> admitted -> preparing -> running
                                                                  -> terminal
```

Every durable job owns one reservation. Admission happens only after the
reservation is durably recorded. Terminal completion, denial, cancellation,
and startup reconciliation release it idempotently. A request larger than the
provider's total capacity is rejected; a compatible request that only exceeds
currently available capacity waits on its explicitly selected provider.

`compute capacity [--provider ID] [--json]` shows capacity, active
reservations, and derived availability. `compute jobs [--provider ID]
[--json]` shows job and reservation states. Placement reports static
compatibility separately from temporary capacity availability.

Successful durable-job receipts bind the reservation ID, requested and
reserved resources, and the capacity snapshot recorded by the atomic reserve
operation. This is historical execution evidence, not mutable live capacity.
