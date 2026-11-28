// Values carry a possible witness per origin and, for simple objects, a heap
// identity. A summary uses symbolic origins for parameters and their fields.
export const clean = { flows: new Map() };
const evidenceLimit = 128;
const truncated = { kind: "truncated", message: "Earlier and later flow steps are shown.", locations: [] };

export function value(key, path = [], ref) {
  return { flows: new Map([[key, path]]), ref };
}

export function join(a, b) {
  if (a === b) return a;
  const flows = new Map(a.flows);
  for (const [origin, path] of b.flows) {
    if (!flows.has(origin) || path.length < flows.get(origin).length) flows.set(origin, path);
  }
  return { flows, ref: a.ref === b.ref ? a.ref : undefined };
}

export function concatPath(...segments) {
  const path = segments.flat();
  return path.length <= evidenceLimit ? path : [...path.slice(0, 64), truncated, ...path.slice(-63)];
}

export function append(input, step) {
  return { ...input, flows: new Map([...input.flows].map(([origin, path]) => [origin, concatPath(path, [step])])) };
}

export function parameter(index) {
  const key = `arg:${index}`;
  return value(key, [], key);
}

export function initialField(key) {
  return key.startsWith("arg:") ? value(key) : clean;
}

export function fieldValue(state, key) {
  return state.fields.get(key) ?? initialField(key);
}

export function fork(state) {
  return { locals: new Map(state.locals), fields: new Map(state.fields) };
}

export function merge(a, b) {
  if (!a || !b) return a ?? b;
  const locals = new Map(a.locals);
  for (const [id, val] of b.locals) locals.set(id, join(a.locals.get(id) ?? clean, val));
  for (const [id, val] of a.locals) if (!b.locals.has(id)) locals.set(id, join(val, clean));
  const fields = new Map();
  for (const key of new Set([...a.fields.keys(), ...b.fields.keys()])) fields.set(key, join(fieldValue(a, key), fieldValue(b, key)));
  return { locals, fields };
}

function sameBindings(a, b) {
  if (a.size !== b.size) return false;
  for (const [key, left] of a) {
    const right = b.get(key);
    if (!right || left.ref !== right.ref || left.flows.size !== right.flows.size) return false;
    for (const origin of left.flows.keys()) if (!right.flows.has(origin)) return false;
  }
  return true;
}

export function sameState(a, b) {
  // Witness lengths can grow on a loop edge without changing reachability.
  return sameBindings(a.locals, b.locals) && sameBindings(a.fields, b.fields);
}

export function keyParts(key) {
  const dot = key.indexOf(".");
  return dot < 0 ? [Number(key.slice(4)), undefined] : [Number(key.slice(4, dot)), key.slice(dot + 1)];
}
