import { formatCents, toCents } from "./format.ts";

export interface Row {
  description: string;
  amount: number;
}

/** Sum of the rows in whole cents, each row rounded as it is shown. */
export function totalCents(rows: Row[]): number {
  return rows.reduce((sum, row) => sum + toCents(row.amount), 0);
}

/** One line per row followed by a totals line. */
export function renderInvoice(rows: Row[]): string {
  const lines = rows.map((row) => row.description + ": " + formatCents(toCents(row.amount)));
  lines.push("Total: " + formatCents(totalCents(rows)));
  return lines.join("\n");
}
