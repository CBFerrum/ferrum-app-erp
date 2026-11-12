// ERP's route table, apart from the page that mounts it so a test can mount the real thing.

import { useEffect } from "react";
import { Link, Navigate, useRouteError, type RouteObject } from "react-router";
import { isolate, shell, t } from "@ferrum/client";
import { StatePanel } from "@ferrum/kit";
import { LoadFailed } from "./states";
import { recordsRoutes, RequireAuth } from "@ferrum/records/frame";

/** ERP's packages and its add-ons': the sidebar shows their modules (Studio has its own interface). */
export const ERP_PACKAGES = [
  ...["core", "parties", "products", "accounting", "selling", "buying", "inventory", "manufacturing"],
  ...["india", "finance", "procurement", "stock_ops", "mfg_ops", "quality"],
];

export function RouteError() {
  return <LoadFailed error={useRouteError()} onRetry={() => window.location.reload()} />;
}

export function NotFound({ back = t("Go to ERP's home") }: { back?: string }) {
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
export function Started() {
  useEffect(() => shell().ready(), []);
  return null;
}

export const erpRoutes: RouteObject[] = [
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
      // A record that has a desk opens on it, its own form a tab away at /f/; a desk without a record answers its entity's
      // list (the sidebar row, shown only to people who can read it), the plain list at /l/. New records use the plain form.
      { path: "e/PaymentRun/:id", lazy: () => import("./PaymentRun").then((m) => ({ Component: m.PaymentRunDesk })) },
      { path: "e/TaxReturn/:id", lazy: () => import("./TaxReturn").then((m) => ({ Component: m.TaxReturnDesk })) },
      { path: "e/CarrierShipment/new", element: <Navigate to="/f/CarrierShipment/new" replace /> },
      { path: "e/CarrierShipment/:id", lazy: () => import("./Shipping").then((m) => ({ Component: m.ShipmentDesk })) },
      { path: "e/CarrierShipment", lazy: () => import("./Shipping").then((m) => ({ Component: m.ShippingHome })) },
      { path: "e/Promotion/new", element: <Navigate to="/f/Promotion/new" replace /> },
      { path: "e/Promotion/:id", lazy: () => import("./Promotions").then((m) => ({ Component: m.Promotions })) },
      { path: "e/Promotion", lazy: () => import("./Promotions").then((m) => ({ Component: m.Promotions })) },
      { path: "e/MrpRun/new", element: <Navigate to="/f/MrpRun/new" replace /> },
      { path: "e/MrpRun/:id", lazy: () => import("./Planning").then((m) => ({ Component: m.PlanningRun })) },
      { path: "e/MrpRun", lazy: () => import("./Planning").then((m) => ({ Component: m.Planning })) },
      { path: "e/StoredValueAccount", lazy: () => import("./StoredValue").then((m) => ({ Component: m.StoredValue })) },
      { path: "e/WithholdingCategory", lazy: () => import("./Withholding").then((m) => ({ Component: m.Withholding })) },
      { path: "e/NumberSeries/new", lazy: () => import("./Numbering").then((m) => ({ Component: m.NewSeries })) },
      { path: "e/NumberSeries", lazy: () => import("./Numbering").then((m) => ({ Component: m.Numbering })) },
      { path: "f/:entity/:id", lazy: () => import("./Form").then((m) => ({ Component: m.FormPage })) },
      { path: "l/:entity", lazy: () => import("./Form").then((m) => ({ Component: m.ListPage })) },
      { path: "e/PaymentRun/new", lazy: () => import("./PaymentRun").then((m) => ({ Component: m.PaymentRunNew })) },
      { path: "desk/payment-run/:id", lazy: () => import("./PaymentRun").then((m) => ({ Component: m.PaymentRunDesk })) },
      { path: "desk/stored-value", lazy: () => import("./StoredValue").then((m) => ({ Component: m.StoredValue })) },
      { path: "e/StoredValueAccount/new", lazy: () => import("./StoredValue").then((m) => ({ Component: m.StoredValue })) },
      { path: "desk/promotions/:id?", lazy: () => import("./Promotions").then((m) => ({ Component: m.Promotions })) },
      { path: "e/TaxReturn/new", lazy: () => import("./TaxReturn").then((m) => ({ Component: m.TaxReturnStart })) },
      { path: "desk/tax-return", lazy: () => import("./TaxReturn").then((m) => ({ Component: m.TaxReturnStart })) },
      { path: "desk/tax-return/:id", lazy: () => import("./TaxReturn").then((m) => ({ Component: m.TaxReturnDesk })) },
      { path: "desk/withholding", lazy: () => import("./Withholding").then((m) => ({ Component: m.Withholding })) },
      { path: "desk/shipping", lazy: () => import("./Shipping").then((m) => ({ Component: m.ShippingHome })) },
      { path: "desk/shipping/:id", lazy: () => import("./Shipping").then((m) => ({ Component: m.ShipmentDesk })) },
      { path: "desk/planning", lazy: () => import("./Planning").then((m) => ({ Component: m.Planning })) },
      { path: "desk/numbering", lazy: () => import("./Numbering").then((m) => ({ Component: m.Numbering })) },
      ...recordsRoutes,
      { path: "*", element: <NotFound /> },
    ],
  },
];

