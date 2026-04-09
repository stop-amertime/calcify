/* tslint:disable */
/* eslint-disable */

/**
 * The main engine handle exposed to JavaScript.
 */
export class CalciteEngine {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Detect video memory region from the CSS structure.
     *
     * Returns a JSON string like `{"addr":753664,"size":4000,"width":80,"height":25}`
     * if video memory is detected, or `"null"` otherwise.
     */
    detect_video(): string;
    /**
     * Get the current value of a register (for debugging).
     */
    get_register(index: number): number;
    /**
     * Return string properties as a JSON object string, e.g. `{"textBuffer":"Hello"}`.
     */
    get_string_properties(): string;
    /**
     * Create a new engine instance from CSS source text.
     */
    constructor(css: string);
    /**
     * Read text-mode video memory (character bytes only).
     *
     * Returns `width * height` bytes from video memory at `base_addr`.
     * Default for DOS text mode: `read_video_memory(0xB8000, 40, 25)`.
     */
    read_video_memory(base_addr: number, width: number, height: number): Uint8Array;
    /**
     * Render text-mode video memory as a string (for debugging).
     */
    render_screen(base_addr: number, width: number, height: number): string;
    /**
     * Set the keyboard input state.
     * Pass (scancode << 8 | ascii), or 0 for no key.
     */
    set_keyboard(key: number): void;
    /**
     * Run a batch of ticks and return the property changes as a JSON string.
     *
     * Returns `[[name, value], ...]` pairs.
     */
    tick_batch(count: number): string;
}

/**
 * Initialise the WASM module (sets up logging, etc.).
 */
export function init(): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_calciteengine_free: (a: number, b: number) => void;
    readonly calciteengine_detect_video: (a: number) => [number, number];
    readonly calciteengine_get_register: (a: number, b: number) => number;
    readonly calciteengine_get_string_properties: (a: number) => [number, number];
    readonly calciteengine_new: (a: number, b: number) => [number, number, number];
    readonly calciteengine_read_video_memory: (a: number, b: number, c: number, d: number) => [number, number];
    readonly calciteengine_render_screen: (a: number, b: number, c: number, d: number) => [number, number];
    readonly calciteengine_set_keyboard: (a: number, b: number) => void;
    readonly calciteengine_tick_batch: (a: number, b: number) => [number, number, number, number];
    readonly init: () => void;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
