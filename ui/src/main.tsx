// ERP's own interface at /erp/ (Ferrum docs/plan/apps-ui.md §13, erp-repo.md): ERP's records and
// pages in the records frame under the platform's bar. The sidebar shows ERP's modules; other apps
// are in the bar's launcher, and the shell's pages (apps, administration, the account) are page
// loads away. The same release draws its pages of the workspace's portal, a customer's "Your
// invoices" at /portal/invoices/ and a supplier's "Your orders" at /portal/orders/ (§9.1, U7).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, Link, Outlet, RouterProvider, useRouteError, type RouteObject } from "react-router";
import { addCatalogs, bootstrap, isolate, routeBase, shell, t, wordsReady } from "@ferrum/client";
import { ConfirmHost, StatePanel, Toaster } from "@ferrum/kit";
import { LoadFailed } from "./states";
import { recordsRoutes, RequireAuth, setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

// Its own words in the page's language (src/locales), fetched as the page starts; it is drawn once they are in.
void addCatalogs({ ar: () => import("./locales/ar.json"), hi: () => import("./locales/hi.json") });

/** ERP's packages and its add-ons': the sidebar shows their modules (Studio has its own interface). */
const ERP_PACKAGES = [
  ...["core", "parties", "products", "accounting", "selling", "buying", "inventory", "manufacturing"],
  ...["india", "finance", "procurement", "stock_ops", "mfg_ops", "quality"],
];

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 30_000 } },
});

function RouteError() {
  return <LoadFailed error={useRouteError()} onRetry={() => window.location.reload()} />;
}

function NotFound({ back = t("Go to ERP's home") }: { back?: string }) {
  return (
    <StatePanel kind="empty" title={t("Page not found")} action={<Link to="/">{isolate(back)}</Link>} />
  );
}

/** ERP's home: its modules, with the start-up signal; loaded when first opened, as every page. */
const erpHome = () =>
  import("./home").then(({ Home }) => ({
    Component: function ErpHome() {
      useEffect(() => shell().ready(), []);
      return <Home />;
    },
  }));

/** Every page tells the shell it started (the home does too; once is enough). */
function Started() {
  useEffect(() => shell().ready(), []);
  return null;
}

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

const erpRoutes = [
  {
    path: "/",
    element: (
      <>
        <Started />
        <RequireAuth />
      </>
    ),
    errorElement: <RouteError />,
    children: [
      { index: true, lazy: erpHome },
      // The accountant's screens answer where the sidebar already points (its reports, the New
      // button of its lists), ahead of the frame's generic pages; each loads when first opened.
      { path: "r/bank_reconciliation", lazy: () => import("./Bank").then((m) => ({ Component: m.Bank })) },
      { path: "r/finance_overview", lazy: () => import("./Today").then((m) => ({ Component: m.Today })) },
      { path: "r/profit_and_loss", lazy: () => import("./Statement").then((m) => ({ Component: m.ProfitAndLoss })) },
      { path: "r/balance_sheet", lazy: () => import("./Statement").then((m) => ({ Component: m.BalanceSheet })) },
      { path: "r/cash_flow", lazy: () => import("./Statement").then((m) => ({ Component: m.CashFlow })) },
      { path: "e/ExchangeRevaluation/new", lazy: () => import("./Revalue").then((m) => ({ Component: m.Revalue })) },
      { path: "e/PaymentAllocation/new", lazy: () => import("./Allocate").then((m) => ({ Component: m.Allocate })) },
      // The desks: each loads when first opened, and answers where the record's own page would (a new record) or beside it.
      { path: "e/PaymentRun/new", lazy: () => import("./PaymentRun").then((m) => ({ Component: m.PaymentRunNew })) },
      { path: "desk/payment-run/:id", lazy: () => import("./PaymentRun").then((m) => ({ Component: m.PaymentRunDesk })) },
      ...recordsRoutes,
      { path: "*", element: <NotFound /> },
    ],
  },
];

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
