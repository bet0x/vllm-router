import type { MetricSample, ParsedMetrics } from './types';

/**
 * Parse Prometheus text exposition format into typed samples.
 *
 *   # TYPE foo counter
 *   foo{bar="baz"} 42
 */
export function parsePrometheus(text: string): ParsedMetrics {
  const samples: MetricSample[] = [];
  const types: Record<string, string> = {};

  for (const line of text.split('\n')) {
    const trimmed = line.trim();

    if (trimmed.startsWith('# TYPE ')) {
      const parts = trimmed.slice(7).split(/\s+/);
      if (parts.length >= 2) types[parts[0]] = parts[1];
      continue;
    }

    if (!trimmed || trimmed.startsWith('#')) continue;

    // name{labels} value   or   name value
    const braceIdx = trimmed.indexOf('{');
    let name: string;
    let labelsStr = '';
    let rest: string;

    if (braceIdx !== -1) {
      name = trimmed.slice(0, braceIdx);
      const closeIdx = trimmed.indexOf('}', braceIdx);
      labelsStr = trimmed.slice(braceIdx + 1, closeIdx);
      rest = trimmed.slice(closeIdx + 1).trim();
    } else {
      const spaceIdx = trimmed.indexOf(' ');
      if (spaceIdx === -1) continue;
      name = trimmed.slice(0, spaceIdx);
      rest = trimmed.slice(spaceIdx + 1).trim();
    }

    const value = parseFloat(rest);
    if (isNaN(value)) continue;

    const labels: Record<string, string> = {};
    if (labelsStr) {
      // parse key="value" pairs
      const re = /(\w+)="([^"]*)"/g;
      let m: RegExpExecArray | null;
      while ((m = re.exec(labelsStr)) !== null) {
        labels[m[1]] = m[2];
      }
    }

    samples.push({ name, labels, value });
  }

  return { samples, types };
}

/** Get value of a simple metric (no labels). */
export function getGauge(parsed: ParsedMetrics, name: string): number | undefined {
  return parsed.samples.find((s) => s.name === name && Object.keys(s.labels).length === 0)?.value;
}

/** Get all samples for a metric name (with any labels). */
export function getSamples(parsed: ParsedMetrics, name: string): MetricSample[] {
  return parsed.samples.filter((s) => s.name === name);
}

/** Get a single sample matching name + exact labels. */
export function getSample(
  parsed: ParsedMetrics,
  name: string,
  labels: Record<string, string>,
): number | undefined {
  return parsed.samples.find(
    (s) =>
      s.name === name &&
      Object.entries(labels).every(([k, v]) => s.labels[k] === v),
  )?.value;
}

/** Compute percentile from histogram buckets. */
export function histogramPercentile(
  parsed: ParsedMetrics,
  name: string,
  percentile: number,
  extraLabels: Record<string, string> = {},
): number | undefined {
  const bucketName = `${name}_bucket`;
  const buckets = parsed.samples
    .filter(
      (s) =>
        s.name === bucketName &&
        Object.entries(extraLabels).every(([k, v]) => s.labels[k] === v),
    )
    .map((s) => ({ le: s.labels.le === '+Inf' ? Infinity : parseFloat(s.labels.le), count: s.value }))
    .sort((a, b) => a.le - b.le);

  if (buckets.length === 0) return undefined;

  const total = buckets[buckets.length - 1].count;
  if (total === 0) return 0;

  const target = total * (percentile / 100);

  for (let i = 0; i < buckets.length; i++) {
    if (buckets[i].count >= target) {
      if (i === 0) return buckets[0].le === Infinity ? 0 : buckets[0].le;
      const prev = buckets[i - 1];
      const curr = buckets[i];
      if (curr.le === Infinity) return prev.le;
      // Linear interpolation
      const fraction = (target - prev.count) / (curr.count - prev.count);
      return prev.le + fraction * (curr.le - prev.le);
    }
  }

  return buckets[buckets.length - 1].le;
}
