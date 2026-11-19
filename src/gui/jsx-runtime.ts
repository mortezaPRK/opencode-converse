// Node-side compatibility shim for the GUI's Solid JSX runtime. The Electron
// host supplies its own DOM runtime; tests only need to load the compiled GUI
// without evaluating DOM elements.
export { Fragment, jsx, jsxDEV, jsxs } from "solid-js/h/jsx-runtime";
export type { JSX } from "solid-js";
