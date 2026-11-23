/** Formats an amount in dollars as text such as `$1,234.50`. */
export function formatMoney(amount: number): string {
  const cents = Math.round(amount * 100);
  const whole = Math.trunc(cents / 100);
  const fraction = Math.abs(cents % 100);
  return "$" + whole + "." + String(fraction).padStart(2, "0");
}
