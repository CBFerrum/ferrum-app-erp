// Warehouse Management's own interface at /wms/ (Ferrum docs/plan/industrial.md): what needs doing,
// the scanner screens for each kind of work, what is in a bin, and the records in lists and forms.

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode, useEffect } from "react";
import { createRoot } from "react-dom/client";
import { createBrowserRouter, Link, RouterProvider, useRouteError } from "react-router";
import { routeBase, shell } from "@ferrum/client";
import { ConfirmHost, LoadFailed, StatePanel, Toaster } from "@ferrum/kit";
import { RequireAuth, setFrameMode } from "@ferrum/records/frame";
import "./styles.css";

const page = (name: "FormView" | "ListView" | "ReportView" | "AppSettingsPage" | "ScreenPage") => () => import("./records").then((m) => ({ Component: m[name] }));

const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 5_000 } } });

function RouteError() {
  return <LoadFailed error={useRouteError()} onRetry={() => window.location.reload()} />;
}

function NotFound() {
  return (
    <StatePanel kind="empty" title="Page not found" action={<Link className="btn btn-primary btn-md" to="/">Go to the warehouse</Link>} />
  );
}

function Started() {
  useEffect(() => shell().ready(), []);
  return null;
}

setFrameMode("app", ["wms"]);

const router = createBrowserRouter(
  [
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
        { index: true, lazy: () => import("./Home").then((m) => ({ Component: m.Home })) },
        { path: "t/:id", lazy: () => import("./Task").then((m) => ({ Component: m.TaskPage })) },
        { path: "receive/:id", lazy: () => import("./Receive").then((m) => ({ Component: m.Receive })) },
        { path: "ship/:id", lazy: () => import("./Ship").then((m) => ({ Component: m.Ship })) },
        { path: "count/:id", lazy: () => import("./Count").then((m) => ({ Component: m.Count })) },
        { path: "bin", lazy: () => import("./Bin").then((m) => ({ Component: m.BinLookup })) },
        { path: "e/:entity", lazy: page("ListView") },
        { path: "e/:entity/:id", lazy: page("FormView") },
        { path: "r/:name", lazy: page("ReportView") },
        { path: "apps/:app/settings", lazy: page("AppSettingsPage") },
        { path: "x/:app/:page", lazy: page("ScreenPage") },
        { path: "*", element: <NotFound /> },
      ],
    },
  ],
  { basename: routeBase("wms") },
);

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
      <Toaster />
      <ConfirmHost />
    </QueryClientProvider>
  </StrictMode>,
);
