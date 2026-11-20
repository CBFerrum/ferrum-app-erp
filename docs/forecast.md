# Demand forecasts

A **Demand Forecast** (Manufacturing) gives each item's expected demand per month in one warehouse, worked out from
what was sold from it. A requirements run (MRP Run) that names it plans for that demand as well as for open orders.

## History
The months read are the **History (Months)** full months before the current one. For each stock item (of the chosen
item group and its subgroups, when one is given), what left the warehouse each month on delivery notes and on sales
invoices that update stock, in stock units. A cancellation or a return is a stock move the other way, so it nets out.
A month with nothing is zero. At most 2,000 items per forecast.

## The three methods
The forecast covers the current month and the **Months Ahead** after it; each figure is rounded to three decimals and
never below zero.

- **Moving Average**: the mean of the last three months. History 10, 20, 25 → 18.333 for every month ahead.
- **Exponential Smoothing**: the level starts at the first month and each month after moves it by **Smoothing** (α)
  towards that month: L = α·x + (1 − α)·L. History 10, 20, 40 with α 0.3 → 10, 13, 21.1: 21.1 for every month ahead.
- **Seasonal**: needs 24 months. Each calendar month's factor is its mean over the history divided by the overall mean;
  the level is the mean of the last twelve months; a month ahead is level × its month's factor. Two years of 10 a month
  with December at 30 → December 30, January 10.

**Plan For** starts as the forecast; change it to plan for another figure. Working the forecast out again keeps every
figure a person changed and replaces the rest.

## How orders use the forecast up
Within a month, open sales orders use up the forecast: only what is left is extra demand, so an order is never planned
twice.
- A run without buckets adds, per item, the forecast months up to its demand date, less the item's open sales order
  demand, needed by the first forecast month (or today).
- A run by day, week or month adds, per item and month, that month's figure less that month's open order demand of the
  item in the forecast's warehouse, on the month's first day (or today); only when the forecast's warehouse is one of
  the run's.

A forecast changes no stock and posts nothing.
