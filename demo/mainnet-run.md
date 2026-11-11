# Compromised agent, mainnet run 2026-11-11 10:36 UTC

Contract lp.kinuonnear.near v1.8.0. Agent key held by the attacker: `da9447d9213a583e8610c872a06ab5b0c2bda245142b73aa454e71d9de112f15`. Victim: `a27c9945554e1a8e4afa8a448b0f5e413b27a1f9a182ba204c2e277c26ea93f6`, lpt `17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1|wrap.near|100#27606`, pool point 401533.

| Attempt | Result | Contract answer | Proof |
| --- | --- | --- | --- |
| Move the funds to another pool | REFUSED | `not in the owner's allowed list` | [tx](https://nearblocks.io/txns/9FaVq1CSC3VPUGiJrNKBAqshNPn6keZif1tV2oT3dsR9) |
| Range wider than the owner allows | REFUSED | `wider than the owner` | [tx](https://nearblocks.io/txns/9xC5SrnMeiY6YjitB2wMsrwVWejgJov9GvphMvkeQRZJ) |
| Range narrower than the minimum | REFUSED | `narrower than the owner` | [tx](https://nearblocks.io/txns/6A5ALH6gVhXm5VMncQd5x2yeKF7SMZqWTYVTku1R66Rg) |
| Range far from the price | REFUSED | `too far from the pool` | [tx](https://nearblocks.io/txns/Byv2Xbk71vrUEbPQJzpBwRwGsm8Ks59p7WgAAVi7hQuN) |
| Churn a position for fees and gas | REFUSED | `not far enough outside` | [tx](https://nearblocks.io/txns/3vZJprs8Lp8Q9BPjcMsybfxPnJXNMtWSqC7hU7RjkD78) |
| Close the position (no exit allowed) | REFUSED | `has not allowed the agent to exit` | [tx](https://nearblocks.io/txns/9Uxk3LeF5erXGKw566Z8r3mzRi6HAL1BhcfwaSNjoAF9) |
| Drop a sale for its own ends | REFUSED | `exactly 1 yoctoNEAR` | [tx](https://nearblocks.io/txns/J2opEWHT87A2GKkAYpBH7UvL9ueWpz7LGDPcmFWGpCkA) |
| Withdraw the tokens | REFUSED | `has not joined` | [tx](https://nearblocks.io/txns/rMnUgQPJf2iJWumWe9rWA5uKFuhEDNedyQxMJZFbnD8) |
| Cash out | REFUSED | `has not joined` | [tx](https://nearblocks.io/txns/8imtVGTtdVZcy9xLR19qiCFGZpEeeKEG9V39AbWDd1MQ) |
| Change the rules | REFUSED | `exactly 1 yoctoNEAR` | [tx](https://nearblocks.io/txns/4Z5xCR8aaQYs5RzGDB7uk8vy7jgCatEzidqq8Vi3EMYF) |
| Name another agent | REFUSED | `exactly 1 yoctoNEAR` | [tx](https://nearblocks.io/txns/4QxYDcj1aLbxFFr6oWnj4rNick4pJZ3k5zYDKRPpCQD1) |
| Take over as owner | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/8rvvNPf6bBrfp3qw5YB4NTRoiHEb9rxBuKd9sudBLN9x) |
| Pause the contract | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/Hm1Gratg2pzXrhMRG9zPsuMJX6Q7cpAyFg6uvdWNcoDN) |
| Pause through the pauser role | REFUSED | `Only the pauser` | [tx](https://nearblocks.io/txns/CyQqcF9V7iXREA6pufiviKCyVZyYbnada1nCFv7yRduM) |
| List its own token | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/7tmi2gLNA8VhYFeewUMf5CrxTPd9CCeochDguwNiCYgt) |
| Trust its own factory | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/23abc5NEVj5CLdZQZVb3XQtPderrhX5ReZ1S3e1jGYTb) |
| Name itself pauser | REFUSED | `Only the owner` | [tx](https://nearblocks.io/txns/4MHtFFSss7RNSCtkb4esp9CCmJd1VvT8DfeXu2khJcL8) |
| Pull the position from Rhea directly | REFUSED | `panicked` | [tx](https://nearblocks.io/txns/HxknWnUPewSZMvmAUcAnfxciQ4aqiAG8uL8vdvC719VM) |

The victim record is byte for byte the same before and after.

Setup, on mainnet with two throwaway implicit accounts: the victim joined with the attacker named as its agent ([tx](https://nearblocks.io/txns/83Zr3NkqrCXwM4RBG2YyBrf7EXGHmYBacfaZzxeLBP6g)), put in 0.1 wNEAR ([tx](https://nearblocks.io/txns/FDjJBSHAFS4JVf3HvapEKWeLL1njeGzpjqP3oqLqEWnU)) and opened a range ([tx](https://nearblocks.io/txns/5Msx7erFcYMaY6SgP2AY8KyrWSEMWSKGzK471ig7k7v1)). So the attacker held a valid agent key for that record, as a thief who took it would.

Some refusals fail the transaction. Others are refused in a callback: the explorer shows the transaction as successful, its log has a `refused` event, and the position does not change (range wider, narrower, far from the price, and churn).

`Drop a sale`, `Change the rules` and `Name another agent` need the user's own one-yocto signature (v1.7), so a key that can only call without a deposit is refused before anything is read. Withdraw and cash out are refused because the attacker has no record: a payout only ever goes to the record's own wallet.

Rhea refused the direct pull with `E100: account not registered`: the attacker has no Rhea account. As in the 30 Oct run, Rhea's own ownership check was not reached here.

Afterwards the victim took everything out with `cash_out(leave)` ([tx](https://nearblocks.io/txns/AEZeeKxxXtK3bG254PqfbDAUpCnSe1rw5USEaLhTpWqH)). Both test accounts were then deleted and their balance went back to kinuonnear.near; on chain both are gone and hold no wNEAR or USDC.

Not run on mainnet: the limit on agent actions per day, because it needs real accepted moves. It is covered by unit and sandbox tests. The real agent and its TEE were not involved: this shows what the contract refuses once an agent key is in the wrong hands. The earlier run on v1.5 (30 Oct 2026, 13 attempts) is in this file's history.
