import type { TimePoint } from './types';

const MAX_POINTS = 120; // ~10 min at 5s interval

/**
 * Ring-buffer time-series store.
 * Each series is identified by a string key.
 */
export class MetricStore {
  private series: Map<string, TimePoint[]> = new Map();

  push(key: string, point: TimePoint) {
    let arr = this.series.get(key);
    if (!arr) {
      arr = [];
      this.series.set(key, arr);
    }
    arr.push(point);
    if (arr.length > MAX_POINTS) arr.shift();
  }

  get(key: string): TimePoint[] {
    return this.series.get(key) ?? [];
  }

  keys(): string[] {
    return [...this.series.keys()];
  }
}
