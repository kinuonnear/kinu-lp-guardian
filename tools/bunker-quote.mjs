#!/usr/bin/env node
// Gets a live NEAR Intents 1Click quote for a Bunker exit and prints the two arguments the vault's
// `bunker_exit` takes: `signed` (the exact string 1Click signed) and `signature`. Nothing is sent.
// Usage: node tools/bunker-quote.mjs <vault account> <token contract> <amount raw> <zcash u1 address>
const [vault, token, amount, recipient] = process.argv.slice(2);
if (!recipient) {
  console.error('usage: node tools/bunker-quote.mjs <vault> <token> <amount> <u1 address>');
  process.exit(1);
}
const body = {
  dry: false, swapType: 'EXACT_INPUT', slippageTolerance: 100,
  originAsset: `nep141:${token}`, depositType: 'ORIGIN_CHAIN', destinationAsset: 'nep141:zec.omft.near', amount,
  refundTo: vault, refundType: 'ORIGIN_CHAIN', recipient, recipientType: 'DESTINATION_CHAIN',
  deadline: new Date(Date.now() + 30 * 60_000).toISOString()
};
const res = await fetch('https://1click.chaindefuser.com/v0/quote', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
const j = await res.json();
if (!j.quoteRequest) {
  console.error(res.status, JSON.stringify(j));
  process.exit(1);
}
// Same field lists and stable JSON as 1Click's SDK verifyQuoteSignature.
const u = (v) => v || undefined;
const stable = (v) => (v === null || typeof v !== 'object' ? JSON.stringify(v) : Array.isArray(v) ? `[${v.map(stable).join(',')}]` : `{${Object.keys(v).sort().filter((k) => v[k] !== undefined).map((k) => `${JSON.stringify(k)}:${stable(v[k])}`).join(',')}}`);
const r = j.quoteRequest;
const q = j.quote;
const signed = stable({
  dry: r.dry, swapType: r.swapType, slippageTolerance: r.slippageTolerance, originAsset: r.originAsset, depositType: r.depositType,
  destinationAsset: r.destinationAsset, amount: r.amount, refundTo: r.refundTo, refundType: r.refundType, recipient: r.recipient,
  recipientType: r.recipientType, deadline: r.deadline, quoteWaitingTimeMs: u(r.quoteWaitingTimeMs), referral: u(r.referral),
  virtualChainRecipient: u(r.virtualChainRecipient), virtualChainRefundRecipient: u(r.virtualChainRefundRecipient), customRecipientMsg: u(r.customRecipientMsg),
  amountIn: q.amountIn, amountInFormatted: q.amountInFormatted, amountInUsd: q.amountInUsd, minAmountIn: q.minAmountIn, amountOut: q.amountOut,
  amountOutFormatted: q.amountOutFormatted, amountOutUsd: q.amountOutUsd, minAmountOut: q.minAmountOut, depositAddress: u(q.depositAddress),
  depositMemo: u(q.depositMemo), deadline: u(q.deadline), timeWhenInactive: u(q.timeWhenInactive), timeEstimate: u(q.timeEstimate),
  refundFee: u(q.refundFee), withdrawFee: u(q.withdrawFee), timestamp: j.timestamp
});
console.log(JSON.stringify({ token, signed, signature: j.signature, amountOutFormatted: q.amountOutFormatted }));
