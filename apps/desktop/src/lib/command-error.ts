/**
 * Extract a human-readable message from a failed command's `error` value.
 *
 * The specta-generated command wrappers return `{ status: "error", error }`
 * in two distinct shapes: a `CommandError` object (`{ kind, message }`) when
 * the Rust command returned `Err`, or whatever raw value the IPC layer
 * rejected with — typically a plain string — when the invoke itself failed
 * (deserialization error, denied capability, panicked command). Accessing
 * `.message` on the string shape yields `undefined` and downstream code
 * that assumes a string (analytics truncation, UI display) throws, masking
 * the real error. Route every `result.error` through this instead.
 */
export function commandErrorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error && typeof error === "object") {
    const message = (error as { message?: unknown }).message;
    if (typeof message === "string") return message;
    try {
      return JSON.stringify(error);
    } catch {
      /* fall through */
    }
  }
  return String(error ?? "unknown error");
}
