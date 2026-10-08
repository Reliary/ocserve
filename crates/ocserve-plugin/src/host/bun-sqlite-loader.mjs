/** Map `bun:sqlite` imports onto node:sqlite (M4b: magic-context needs it). */
export async function resolve(specifier, context, nextResolve) {
  if (specifier === "bun:sqlite") {
    return { url: new URL("./shim-bun-sqlite.mjs", import.meta.url).href, shortCircuit: true };
  }
  return nextResolve(specifier, context);
}
