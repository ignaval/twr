// The server retains receipts for 24 hours. Leave one hour for clock differences/pruning.
export const RECEIPT_WINDOW = 23 * 60 * 60 * 1000;
