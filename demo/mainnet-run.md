# Compromised agent, mainnet run 2026-10-30 23:07 UTC

Contract lp.kinuonnear.near v1.5.0. Agent key held by the attacker: `fe490349882314f7b03e42df8a71dbf0a393bec863519a8460e946ed11095130`. Victim: `1829a0384705c4fc2ec4f5b3a6e58b4139e3610eebd81081fed4e2cdd10a19df`, lpt `17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1|wrap.near|100#24234`, pool point 399797.

| Attempt | Result | Contract answer | Proof |
| --- | --- | --- | --- |
| Move the funds to another pool | REFUSED | `not in the owner's allowed list` | [tx](https://nearblocks.io/txns/9tb94n8mmfiDYt1j95NYL6zWKcgmz9ccimtsmdCXoFyr) |
| Range wider than the owner allows | REFUSED | `wider than the owner` | [tx](https://nearblocks.io/txns/DhjCt1zFALGwX8iN2C3Wtf6gnKkmzhXPohC974WrwYb8) |
| Range narrower than the minimum | REFUSED | `narrower than the owner` | [tx](https://nearblocks.io/txns/5zJJQYdVAHTp3wTKjx9hfn4X7ApibTt618cdhFWce7Zt) |
| Range far from the price | REFUSED | `too far from the pool` | [tx](https://nearblocks.io/txns/7LqDAPvxWVnmoJkfHCHZC2qC42FtTretLMhE7xc22yHk) |
| Churn a position for fees and gas | REFUSED | `not far enough outside` | [tx](https://nearblocks.io/txns/9JCukJax9ru5s8ZG6YsDLJPsXynn8P5oUYBrp8hQzCtm) |
| Close the position (no exit allowed) | REFUSED | `has not allowed the agent to exit` | [tx](https://nearblocks.io/txns/22erjhHjY4iwmXZqQxn1cpQZi1iAXYq3DeYMuMpgkBrT) |
| Withdraw the tokens | REFUSED | `has not joined` | [tx](https://nearblocks.io/txns/9p1Smu5QZw59gfR2zMmU9WygpimhHE2jnk3CPjahfXrA) |
| Cash out | REFUSED | `has not joined` | [tx](https://nearblocks.io/txns/GffEAZeVpfed2jJ87m3mqJMkMTHDRoirXUeKdk7qNKnC) |
| Change the rules | REFUSED | `has not joined` | [tx](https://nearblocks.io/txns/3o6ZkZcLsuHvGnGAZggxeLv3TuphM8XBrp4ieqfUCKZp) |
| Name another agent | REFUSED | `has not joined` | [tx](https://nearblocks.io/txns/9xWhG1zgc6MHxns8bALuMQG5HoNZ8PgEU4DAGtCkAFKQ) |
| Take over as owner | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/BkkKtWpHiX9YpN5m7NqfnzMXkf4ZJ7D5wm1suLM5LMBQ) |
| Pause the contract | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/4KSs6n1GsFujMwRwMFSqXvpLVGoz4qrbYaJe8nC4ubWF) |
| Pull the position from Rhea directly | REFUSED | `E100: account not registered` | [tx](https://nearblocks.io/txns/EfBNpQvPeMbo2vyARAi9zPGbFWeBrfUYYrRfJNqZcLjK) |

The victim record is byte for byte the same before and after.

Some refusals fail the transaction. Others are refused in a callback: the explorer shows the transaction as successful, its log has a `refused` event, and the position does not change (range wider, narrower, far from the price, and churn).

Rhea refused the direct pull because the attacker has no Rhea account. Two tries to register one failed on storage (E102), so Rhea's own ownership check was not reached here.

Afterwards the victim took everything out with `cash_out(leave)` ([tx](https://nearblocks.io/txns/94kVMXoTgrJ38cutXXhWCBTsMHBNqUouLvuKaGHBp2wS)). Both test accounts were then deleted and their balance went back to kinuonnear.near.

Not run on mainnet: the limit on agent actions per day, because it needs real accepted moves. It is covered by unit and sandbox tests. The real agent and its TEE were not involved: this shows what the contract refuses once an agent key is in the wrong hands.
