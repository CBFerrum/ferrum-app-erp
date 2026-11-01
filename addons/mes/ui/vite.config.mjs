// Manufacturing Execution's own interface, built with Ferrum-Frontend's packages and libraries from the
// sibling checkout.
import { ferrumApp } from "../../../../ferrum/web/packages/build/vite.mjs";

export default ferrumApp(import.meta.url);
