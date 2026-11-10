import { formatMoney } from "./format.ts";

export interface Row {
  description: string;
  amount: number;
}

/** Sum of the rows in dollars. */
export function total(rows: Row[]): number {
  return rows.reduce((sum, row) => sum + row.amount, 0);
}

/** One line per row followed by a totals line. */
export function renderInvoice(rows: Row[]): string {
  const lines = rows.map((row) => row.description + ": " + formatMoney(row.amount));
  lines.push("Total: " + formatMoney(total(rows)));
  return lines.join("\n");
}
