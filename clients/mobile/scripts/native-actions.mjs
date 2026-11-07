/** Real click path through the overflow when narrow layouts hide the toolbar action. */
export async function shellAction(page) {
  const control = page.getByRole("button", { name: "Open shell", exact: true });
  if (!(await control.isVisible()))
    await page.locator(".conversation-menu summary").click();
  return control;
}
