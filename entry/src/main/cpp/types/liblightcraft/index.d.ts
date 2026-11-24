export const init: (config: string, callback: (id: number, message: string, data: ArrayBuffer) => void) => void;
export const input: (event: string) => void;
// Answer a pending platform request (id > 0). `text` is the chosen path / JSON result,
// `data` carries binary payloads (e.g. export bytes the Rust side asked to write).
export const reply: (id: number, ok: boolean, text: string, data: ArrayBuffer) => void;
export const active: (active: boolean) => void;
export const close: () => void;
