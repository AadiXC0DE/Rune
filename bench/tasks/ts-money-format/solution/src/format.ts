/** Rounds an amount in dollars to whole cents, halves away from zero. */
export function toCents(amount: number): number {
  const cents = Math.round(Math.abs(amount) * 100);
  return amount < 0 ? -cents : cents;
}

/** Formats whole cents as text such as `$1,234.50` or `-$0.01`. */
export function formatCents(cents: number): string {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(cents);
  const whole = String(Math.floor(abs / 100)).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return sign + "$" + whole + "." + String(abs % 100).padStart(2, "0");
}

/** Formats an amount in dollars as text such as `$1,234.50`. */
export function formatMoney(amount: number): string {
  return formatCents(toCents(amount));
}
