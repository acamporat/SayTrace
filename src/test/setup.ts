import "@testing-library/jest-dom/vitest";
import { afterEach } from "vitest";
import { cleanup } from "@testing-library/react";

afterEach(() => {
  cleanup();
  // Node 25 exposes an incomplete global localStorage when no backing file is
  // configured. JSDOM inherits it in that environment, so clear only when the
  // browser Storage API is actually available.
  if (typeof window.localStorage.clear === "function") {
    window.localStorage.clear();
  }
});
