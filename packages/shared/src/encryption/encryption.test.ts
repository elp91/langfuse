import { describe, expect, it } from "vitest";

import { decrypt, encrypt } from "./encryption";

describe("encrypt/decrypt", () => {
  it("round-trips an empty string", () => {
    // Keyless gateway connections store encrypt("") in a non-nullable column,
    // so the ciphertext must decrypt back to "" instead of being rejected as
    // a corrupted cipher.
    expect(decrypt(encrypt(""))).toBe("");
  });

  it("round-trips a credential", () => {
    expect(decrypt(encrypt("sk-secret"))).toBe("sk-secret");
  });

  it("rejects a malformed ciphertext", () => {
    expect(() => decrypt("x")).toThrow("Invalid or corrupted cipher format");
  });
});
