export function numberSetting(value: unknown): number {
  if (typeof value !== "number" && (typeof value !== "string" || !value.trim()))
    throw new Error("Enter a number before saving this setting.");
  const number = Number(value);
  if (!Number.isSafeInteger(number) || number < 0)
    throw new Error("Enter a whole number of zero or greater.");
  return number;
}
