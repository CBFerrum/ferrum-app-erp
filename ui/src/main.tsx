// ERP's own interface at /erp/ (Ferrum docs/plan/apps-ui.md §13, erp-repo.md): ERP's records and
// pages in the records frame under the platform's bar. The sidebar shows ERP's modules; other apps
// are in the bar's launcher, and the shell's pages (apps, administration, the account) are page
// loads away. The same release draws its pages of the workspace's portal, a customer's "Your
// invoices" at /portal/invoices/ and a supplier's "Your orders" at /portal/orders/ (§9.1, U7).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, Outlet, RouterProvider, type RouteObject } from "react-router";
import { addCatalogs, bootstrap, routeBase, t, wordsReady } from "@ferrum/client";
import { ConfirmHost, Toaster } from "@ferrum/kit";
import { ERP_PACKAGES, erpRoutes, NotFound, RouteError, Started } from "./routes";
import { setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

// Its own words in the page's language (src/locales), fetched as the page starts; it is drawn once they are in.
void addCatalogs({ ar: () => import("./locales/ar.json"), hi: () => import("./locales/hi.json") });

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 30_000 } },
});

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

const router = portal
  ? createBrowserRouter(portalRoutes, { basename: routeBase(entry) })
  : createBrowserRouter(erpRoutes, { basename: routeBase("erp") });

// Drawn once the page's words are in (its language's catalogs, fetched as the page starts).
void wordsReady().then(() =>
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <QueryClientProvider client={queryClient}>
        <RouterProvider router={router} />
        <Toaster />
        <ConfirmHost />
      </QueryClientProvider>
    </StrictMode>,
  ),
);
