/** Fence delayed bootstrap snapshots against terminal operations in this host view. */
export class TerminalSnapshots {
  private revision = 0;
  private closed = new Set<string>();
  begin(): number {
    return this.revision;
  }
  changed(): void {
    this.revision++;
  }
  removed(id: string): void {
    this.closed.add(id);
    this.changed();
  }
  apply<T extends { id: string }>(
    revision: number,
    sessions: T[],
  ): T[] | undefined {
    if (revision !== this.revision) return;
    return sessions.filter((session) => !this.closed.has(session.id));
  }
}
