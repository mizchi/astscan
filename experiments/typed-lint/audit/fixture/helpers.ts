import { input, record } from "./apis";

export function wrap(value: string) {
  const copy = value;
  return copy;
}

export function write(target: { value: string }, value: string) {
  target.value = value;
}

export function observe(target: { value: string }) {
  record(target.value);
}

export async function fetchAsync() {
  return input();
}

export function neverReturns(): never {
  for (;;) {}
}
