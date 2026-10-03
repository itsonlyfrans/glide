// Thin wrapper over the preload bridge. Errors are thrown as { code, message }.
export async function call(method, params = {}) {
  const res = await window.glide.call(method, params);
  if (!res.ok) throw res.error;
  return res.result;
}
export const onEvent = (fn) => window.glide.onEvent(fn);
export const meta = () => window.glide.meta();
