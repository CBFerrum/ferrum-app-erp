// ERP's own interface at /erp/ (Ferrum docs/plan/apps-ui.md §13, erp-repo.md): ERP's records and
// pages in the records frame under the platform's bar. The sidebar shows ERP's modules; other apps
// are in the bar's launcher, and the shell's pages (apps, administration, the account) are page
// loads away. The same release draws its page of the workspace's portal, "Your invoices" at
// /portal/invoices/ (§9.1, U7).

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, Link, Outlet, RouterProvider, useRouteError } from "react-router";
import { bootstrap, routeBase, shell } from "@ferrum/client";
import { ConfirmHost, EmptyState, ErrorState, Toaster } from "@ferrum/kit";
import { recordsRoutes, RequireAuth, setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

/** ERP's packages and its add-ons': the sidebar shows their modules (Studio has its own interface). */
const ERP_PACKAGES = [
  ...["core", "parties", "products", "accounting", "selling", "buying", "inventory", "manufacturing"],
  ...["india", "finance", "procurement", "stock_ops", "mfg_ops"],
];

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 30_000 } },
});

function RouteError() {
  return <ErrorState error={useRouteError()} onRetry={() => window.location.reload()} />;
}

function NotFound({ back = "Go to ERP's home" }: { back?: string }) {
  return (
    <EmptyState title="Page not found">
      <Link to="/">{back}</Link>
    </EmptyState>
  );
}

/** ERP's home: its modules, with the start-up signal; loaded when first opened, as every page. */
const erpHome = () =>
  import("./home").then(({ ModulesHome }) => ({
    Component: function ErpHome() {
      useEffect(() => shell().ready(), []);
      return <ModulesHome />;
    },
  }));

/** Every page tells the shell it started (the home does too; once is enough). */
function Started() {
  useEffect(() => shell().ready(), []);
  return null;
}

/** The portal's page, when the platform serves this release there. */
const PORTAL = "portal/invoices";
const onPortal = bootstrap()?.app?.entry === PORTAL;

if (!onPortal) setFrameMode("app", ERP_PACKAGES, { pages: true });

const portalRoutes = [
  {
    path: "/",
    element: (
      <main className="portal-page">
        <Started />
        <Outlet />
      </main>
    ),
    errorElement: <RouteError />,
    children: [
      { index: true, lazy: () => import("./Invoices").then((m) => ({ Component: m.PortalInvoices })) },
      { path: ":id", lazy: () => import("./Invoices").then((m) => ({ Component: m.PortalInvoice })) },
      { path: "*", element: <NotFound back="Go to your invoices" /> },
    ],
  },
];

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
      { path: "r/profit_and_loss", lazy: () => import("./Statement").then((m) => ({ Component: m.ProfitAndLoss })) },
      { path: "r/balance_sheet", lazy: () => import("./Statement").then((m) => ({ Component: m.BalanceSheet })) },
      { path: "r/cash_flow", lazy: () => import("./Statement").then((m) => ({ Component: m.CashFlow })) },
      { path: "e/PaymentAllocation/new", lazy: () => import("./Allocate").then((m) => ({ Component: m.Allocate })) },
      ...recordsRoutes,
      { path: "*", element: <NotFound /> },
    ],
  },
];

const router = onPortal
  ? createBrowserRouter(portalRoutes, { basename: routeBase(PORTAL) })
  : createBrowserRouter(erpRoutes, { basename: routeBase("erp") });

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
      <Toaster />
      <ConfirmHost />
    </QueryClientProvider>
  </StrictMode>,
);
