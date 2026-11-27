import { fetchRecord as read, lookup, ambiguous, dataWithThenProperty, opaque } from "./api.js";

const label = "🧪 読む"; read();
lookup("async");
lookup("sync");
ambiguous();
dataWithThenProperty();
opaque();

async function explicitlyHandled() {
  await read();
  void read();
  const pending = read();
  return pending;
}

function shadowed() {
  const read = () => label;
  read();
}

function validate(email: string | undefined, role: string) {
  if (!email) {
    throw new Error("missing value");
  }
  if (role !== "admin") {
    throw new Error("missing value");
  } else {
    throw new Error("unexpected role");
  }
}

function earlyExit(email: string | undefined) {
  if (email) return;
  throw new Error("missing value");
}
