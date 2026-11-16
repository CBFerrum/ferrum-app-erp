// ERP's own interface at /erp/ (Ferrum docs/plan/apps-ui.md §13, erp-repo.md): ERP's records and
// pages in the records frame under the platform's bar. The sidebar shows ERP's modules; other apps
// are in the bar's launcher, and the shell's pages (apps, administration, the account) are page
// loads away. The same release draws its pages of the workspace's portal, a customer's "Your
// invoices" at /portal/invoices/ and a supplier's "Your orders" at /portal/orders/ (§9.1, U7).

import { Outlet, type RouteObject } from "react-router";
import { bootstrap, t } from "@ferrum/client";
import { ERP_PACKAGES, erpRoutes, NotFound, RouteError, Started } from "./routes";
import { mountApp, setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

/** The portal's pages, when the platform serves this release there, by their entry. */
const PORTALS: Record<string, { back: string; pages: RouteObject[] }> = {
  "portal/invoices": {
    get back() { return t("Go to your invoices"); },
    pages: [
      { index: true, lazy: () => import("./Invoices").then((m) => ({ Component: m.PortalInvoices })) },
      { path: ":id", lazy: () => import("./Invoices").then((m) => ({ Component: m.PortalInvoice })) },
    ],
  },
  "portal/orders": {
    get back() { return t("Go to your orders"); },
    pages: [
      { index: true, lazy: () => import("./Orders").then((m) => ({ Component: m.PortalOrders })) },
      { path: ":id", lazy: () => import("./Orders").then((m) => ({ Component: m.PortalOrderPage })) },
    ],
  },
};
const entry = bootstrap()?.app?.entry ?? "";
const portal = Object.hasOwn(PORTALS, entry) ? PORTALS[entry] : undefined;

if (!portal) setFrameMode("app", ERP_PACKAGES, { pages: true });

const portalRoutes: RouteObject[] = portal
  ? [
      {
        path: "/",
        element: (
          <main className="portal-page">
            <Started />
            <Outlet />
          </main>
        ),
        errorElement: <RouteError />,
        children: [...portal.pages, { path: "*", element: <NotFound back={portal.back} /> }],
      },
    ]
  : [];

mountApp({
  route: portal ? entry : "erp",
  catalogs: { ar: () => import("./locales/ar.json"), hi: () => import("./locales/hi.json") },
  routes: portal ? portalRoutes : erpRoutes,
});
