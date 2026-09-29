// Small observable stores the React views read with `useSyncExternalStore`, so a view re-renders
// only for the slice it reads: the header's clock moves every frame, and the cards must not.

export class Store<T> {
  private value: T;
  private readonly listeners = new Set<() => void>();

  constructor(initial: T) {
    this.value = initial;
  }

  get(): T {
    return this.value;
  }

  set(next: T): void {
    if (Object.is(next, this.value)) {
      return;
    }
    this.value = next;
    for (const listener of [...this.listeners]) {
      listener();
    }
  }

  update(change: (current: T) => T): void {
    this.set(change(this.value));
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  snapshot = (): T => this.value;
}

/** A counter for state kept in a mutable model: the view re-renders when it moves. */
export class Version extends Store<number> {
  constructor() {
    super(0);
  }

  bump(): void {
    this.set(this.get() + 1);
  }
}
