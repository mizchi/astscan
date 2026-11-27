import { approved, approvedAsync, copy, copyTo, input as read, mask, maskInPlace, opaque, record as log } from "./apis";
import { fetchAsync, neverReturns, observe, wrap, write } from "./helpers";

export function direct() {
  const value = read();
  const α = "🌙";
  log(α + value);
}

export function masked() {
  log(mask(read()));
}

export function oneBranch(flag: boolean) {
  let value = read();
  if (flag) value = mask(value);
  log(value);
}

export function bothBranches(flag: boolean) {
  let value = read();
  if (flag) value = mask(value);
  else value = "default";
  log(value);
}

export function unrelated() {
  const value = read();
  {
    const value = "inner";
    log(value);
  }
}

export function shadowedApi() {
  function log(_value: string) {}
  log(read());
}

export function checked() {
  const value = read();
  if (!approved(value)) return;
  log(value);
}

export function throughReturn() {
  log(wrap(read()));
}

export function throughEffect() {
  const box = { value: "initial" };
  const alias = box;
  write(alias, read());
  observe(box);
}

export function fieldMasked() {
  const box = { value: read() };
  maskInPlace(box);
  log(box.value);
}

export function modeledCopy() {
  log(copy(read()));
}

export function modeledField() {
  const box = { value: "initial" };
  copyTo(read(), box);
  log(box.value);
}

export function throughParameter(value: string) {
  log(value);
}

export function unknownType() {
  const value: any = read();
  log(value);
}

export function unknownCall() {
  log(opaque(read()));
}

export function countedLoop() {
  let value = "initial";
  for (let i = 0; i < 1; i++) value = read();
  log(value);
}

export function loopCarry(flag: boolean) {
  let value = "initial";
  let keep = flag;
  while (keep) {
    log(value);
    if (approved(value)) break;
    value = read();
  }
}

export function loopContinue(flag: boolean) {
  let value = "initial";
  let keep = flag;
  for (; keep; value = read()) {
    keep = false;
    continue;
  }
  log(value);
}

export function loopMasked(flag: boolean) {
  let value = read();
  do value = mask(value); while (flag);
  log(value);
}

export function loopUnreachable() {
  for (;;) {}
  log(read());
}

export function unreachableArgument() {
  log(neverReturns() + read());
}

export function unreachableAfterCall() {
  neverReturns();
  log(read());
}

export function nestedLoopJumps(flag: boolean) {
  let value = read();
  do {
    for (;;) break;
    value = mask(value);
    continue;
    value = read();
  } while (flag);
  log(value);
}

export function unknownIterator(values: string[]) {
  for (const value of values) log(value);
}

export async function throughAwait() {
  log(await fetchAsync());
}

export async function awaitedMasked() {
  log(mask(await (fetchAsync())));
}

export async function awaitedGuard() {
  const value = read();
  if (!(await approvedAsync(value))) return;
  log(value);
}

export function guardPromise() {
  const value = read();
  if (approvedAsync(value)) log(value);
}

export function unawaited() {
  fetchAsync();
}
