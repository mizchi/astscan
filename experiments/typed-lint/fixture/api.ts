export async function fetchRecord(): Promise<number> {
  return 42;
}

export function lookup(mode: "async"): Promise<number>;
export function lookup(mode: "sync"): number;
export function lookup(mode: "async" | "sync"): Promise<number> | number {
  return mode === "async" ? Promise.resolve(42) : 42;
}

export function ambiguous(): PromiseLike<number> | number {
  return Math.random() < 0.5 ? Promise.resolve(42) : 42;
}

export function dataWithThenProperty() {
  return { then: "metadata" };
}

export function opaque(): unknown {
  return 42;
}
