import { useEffect, useMemo, useRef, useState } from 'react';
import { BarChart2, Book, Cpu, DollarSign, TrendingUp } from 'lucide-react';
import type { Snapshot, UsageDay, UsageSummary } from '../types';
import { compactTokens, days, heatmapLevel } from '../utils.mjs';
import { Empty, Spinner } from '../components';
import { errorMessage, request, subscribe } from '../api';

const COST_MODELS = [
  { id: 'sonnet', name: 'Claude Sonnet 5.5', input: 2, cachedInput: 0.2, output: 10 },
  { id: 'opus', name: 'Claude Opus 5.5', input: 4, cachedInput: 0.2, output: 20 },
];

/// Well-known texts to measure token counts against. English runs at about 1.3 tokens per word.
const TOKENS_PER_WORD = 1.3;
const WORKS: [string, number][] = [
  ['The Great Gatsby', 47_000],
  ['Moby-Dick', 206_000],
  ['The Lord of the Rings', 480_000],
  ['War and Peace', 587_000],
  ['the King James Bible', 783_000],
  ['the complete works of Shakespeare', 884_000],
  ['all seven Harry Potter books', 1_084_000],
  ['the whole English Wikipedia', 4_500_000_000],
];

const tokensOf = (words: number) => words * TOKENS_PER_WORD;
const percent = (ratio: number) => `${Math.min(99, Math.max(1, Math.round(ratio * 100)))}%`;
const times = (ratio: number) =>
  ratio >= 100 ? `${Math.round(ratio)}×` : ratio >= 10 ? `${Math.round(ratio)}×` : `${ratio.toFixed(1)}×`;

/// The work the headline uses: the largest one the total covers at least once, else the smallest.
const headlineWork = (total: number): [string, number] =>
  [...WORKS].reverse().find(([, words]) => total >= tokensOf(words)) ?? WORKS[0];

const headline = (total: number) => {
  const [name, words] = headlineWork(total);
  const ratio = total / tokensOf(words);
  return total < words * TOKENS_PER_WORD
    ? `about ${percent(ratio)} of ${name}`
    : `about ${times(ratio)} ${name}`;
};

const others = (total: number) => {
  const shown = headlineWork(total)[0];
  return WORKS.filter(
    ([name, words]) =>
      name !== shown && total >= tokensOf(words) * 0.05 && total < tokensOf(words) * 5_000,
  )
    .reverse()
    .slice(0, 4)
    .map(([name, words]) => {
      const ratio = total / tokensOf(words);
      return ratio >= 1 ? `${times(ratio)} ${name}` : `${percent(ratio)} of ${name}`;
    });
};

const costFor = (usage: UsageDay, model: (typeof COST_MODELS)[number]) => {
  const uncached = Math.max(0, usage.prompt - usage.cached);
  return (uncached * model.input + usage.cached * model.cachedInput + usage.output * model.output) / 1_000_000;
};

const formatCost = (cost: number) =>
  cost >= 100 ? `$${Math.round(cost)}` : `$${cost.toFixed(2)}`;

const orderedBreakdown = (items: Record<string, number>) =>
  Object.entries(items)
    .filter(([, tokens]) => tokens > 0)
    .sort((a, b) => (a[1] !== b[1] ? b[1] - a[1] : a[0].localeCompare(b[0])));

const breakdownRows = (items: Record<string, number>) => {
  const ordered = orderedBreakdown(items);
  return ordered.length <= 6
    ? ordered
    : [...ordered.slice(0, 5), [`${ordered.length - 5} others`, ordered.slice(5).reduce((sum, [, tokens]) => sum + tokens, 0)]];
};

function ActivityGrid({ summary }: { summary: UsageSummary }) {
  const ref = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(800);

  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    const observer = new ResizeObserver((entries) => {
      for (const entry of entries) setWidth(Math.floor(entry.contentRect.width));
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  const { weeks, used, year } = useMemo(() => {
    const today = new Date();
    today.setHours(0, 0, 0, 0);
    const weekday = today.getDay();
    const start = new Date(today);
    start.setDate(today.getDate() - weekday);
    start.setDate(start.getDate() - 52 * 7);

    const weeks: (Date | null)[][] = [];
    for (let week = 0; week < 53; week++) {
      const column: (Date | null)[] = [];
      for (let row = 0; row < 7; row++) {
        const date = new Date(start);
        date.setDate(start.getDate() + week * 7 + row);
        column.push(date > today ? null : date);
      }
      weeks.push(column);
    }
    const key = (date: Date) => date.toISOString().slice(0, 10);
    const shown = weeks.flat().flatMap((date) => (date ? [summary.days[key(date)]?.total ?? 0] : []));
    const used = shown.filter((total: number) => total > 0).sort((a: number, b: number) => a - b);
    const year = shown.reduce((sum: number, total: number) => sum + total, 0);
    return { weeks, used, year };
  }, [summary]);

  const cell = Math.max(6, Math.min(16, (width - 26 - 3 * 53) / 53));
  const monthNames = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const weekdayNames = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
  const key = (date: Date) => date.toISOString().slice(0, 10);

  return (
    <div className="usage-activity" ref={ref}>
      <div className="usage-activity-head">
        <strong>{compactTokens(year)} tokens in the last year</strong>
        <div className="usage-legend">
          <span>Less</span>
          {[0, 1, 2, 3, 4].map((level) => (
            <i key={level} className="usage-legend-cell" style={{ background: levelColor(level) }} />
          ))}
          <span>More</span>
        </div>
      </div>
      <div className="usage-heatmap">
        <div className="usage-months">
          <span className="usage-corner" />
          {weeks.map((_, index) => {
            const first = weeks[index].find(Boolean);
            const starts = index < weeks.length - 2 && first ? first.getDate() <= 7 : false;
            return (
              <span key={index} className="usage-month">
                {starts ? monthNames[first!.getMonth()] : ''}
              </span>
            );
          })}
        </div>
        <div className="usage-body">
          <div className="usage-weekdays">
            {Array.from({ length: 7 }, (_, row) => {
              const weekday = (7 - 7 + row) % 7;
              const show = [1, 3, 5].includes(weekday);
              return (
                <span key={row} className="usage-weekday" style={{ height: cell }}>
                  {show ? weekdayNames[weekday] : ''}
                </span>
              );
            })}
          </div>
          <div className="usage-columns">
            {weeks.map((column, index) => (
              <div key={index} className="usage-column">
                {column.map((date, row) => {
                  if (!date) return <span key={row} className="usage-cell empty" />;
                  const total = summary.days[key(date)]?.total ?? 0;
                  const level = heatmapLevel(total, used);
                  const label = `${date.toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric' })}: ${total > 0 ? `${compactTokens(total)} tokens in ${summary.days[key(date)]!.requests} requests` : 'no use'}`;
                  return (
                    <span
                      key={row}
                      className="usage-cell"
                      style={{ background: levelColor(level) }}
                      title={label}
                    />
                  );
                })}
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}

function levelColor(level: number) {
  if (level === 0) return 'var(--surface-3)';
  const opacities = [0, 0.3, 0.5, 0.75, 1];
  return `color-mix(in srgb, var(--green) ${opacities[level] * 100}%, transparent)`;
}

function Breakdown({ title, items }: { title: string; items: Record<string, number> }) {
  const rows = breakdownRows(items);
  return (
    <div className="usage-breakdown">
      <div className="usage-breakdown-head">
        <span className="usage-breakdown-icon"><Cpu size={15} /></span>
        <span className="usage-breakdown-title">{title}</span>
      </div>
      {rows.length === 0 ? (
        <p className="usage-breakdown-empty">Nothing yet.</p>
      ) : (
        rows.map(([name, tokens]) => (
          <div key={name} className="usage-breakdown-row">
            <span className="usage-breakdown-name">{name}</span>
            <span className="usage-breakdown-value">{compactTokens(tokens)}</span>
          </div>
        ))
      )}
    </div>
  );
}

export default function UsageView({ snapshot }: { snapshot: Snapshot }) {
  const [summary, setSummary] = useState<UsageSummary | null>(null);
  const [costModel, setCostModel] = useState('sonnet');
  const [error, setError] = useState('');

  useEffect(() => {
    void request<UsageSummary>('usage.get').then(setSummary).catch((failure) => setError(errorMessage(failure)));
  }, []);

  useEffect(() => {
    let alive = true;
    subscribe((event) => {
      if (event.type !== 'usage.recorded' || !event.data) return;
      void request<UsageSummary>('usage.get').then((next) => { if (alive) setSummary(next); }).catch((failure) => { if (alive) setError(errorMessage(failure)); });
    }).catch((failure) => { if (alive) setError(errorMessage(failure)); });
    return () => { alive = false; };
  }, []);

  const all = summary?.all;
  const activeDays = summary
    ? Object.values(summary.days).filter((day) => day.total > 0).length
    : 0;
  const cachePercent = all && all.prompt > 0 ? Math.min(100, Math.round((all.cached / all.prompt) * 100)) : null;
  const model = COST_MODELS.find((candidate) => candidate.id === costModel) ?? COST_MODELS[0];

  if (error) {
    return (
      <div className="page-content usage-content">
        <div className="page-title"><div><div className="eyebrow">LOCAL USAGE</div><h1>Usage</h1><p>Couldn't load usage: {error}</p></div></div>
      </div>
    );
  }

  return (
    <div className="page-content usage-content">
      <div className="page-title">
        <div>
          <div className="eyebrow">LOCAL USAGE</div>
          <h1>Usage <span className="count-badge"><BarChart2 size={14} /></span></h1>
          <p>Every token your local models read and wrote. Counted on this Mac and never sent anywhere.</p>
        </div>
      </div>

      {!all || all.requests === 0 ? (
        <Empty icon={<BarChart2 size={26} />} title="No Usage Yet">
          Load a model and use it from an agent, a task or the chat. Every request is counted here from now on.
        </Empty>
      ) : (
        <>
          <div className="usage-metrics">
            <div className="metric">
              <span className="metric-label"><Cpu size={14} /> Tokens</span>
              <strong>{compactTokens(all.total)}</strong>
              <small>{compactTokens(all.prompt)} read · {compactTokens(all.output)} written</small>
            </div>
            <div className="metric">
              <span className="metric-label"><TrendingUp size={14} /> Requests</span>
              <strong>{all.requests}</strong>
              <small>{days(activeDays)} active</small>
            </div>
            <div className="metric">
              <span className="metric-label"><Book size={14} /> Prompt Cache</span>
              <strong>{cachePercent !== null ? `${cachePercent}%` : '—'}</strong>
              <small>{compactTokens(all.cached)} tokens reused</small>
            </div>
            <div className="metric">
              <span className="metric-label"><DollarSign size={14} /> Est. Cloud Cost</span>
              <strong>{all.total > 0 ? formatCost(costFor(all, model)) : '$0.00'}</strong>
              <small>On a {model.name} API</small>
            </div>
          </div>

          <ActivityGrid summary={summary} />

          <div className="usage-cards">
            <div className="usage-card usage-comparison">
              <div className="usage-breakdown-head">
                <span className="usage-breakdown-icon"><Book size={15} /></span>
                <span className="usage-breakdown-title">That's like reading</span>
              </div>
              <div className="usage-comparison-headline">{headline(all.total)}</div>
              <ul className="usage-comparison-list">
                {others(all.total).map((line) => (
                  <li key={line}>{line}</li>
                ))}
              </ul>
              <p className="usage-note">Counting about 1.3 tokens per English word.</p>
            </div>

            <div className="usage-card usage-cost">
              <div className="usage-breakdown-head">
                <span className="usage-breakdown-icon"><DollarSign size={15} /></span>
                <span className="usage-breakdown-title">On a cloud API</span>
                <select className="usage-cost-select" value={costModel} onChange={(event) => setCostModel(event.target.value)}>
                  {COST_MODELS.map((candidate) => (
                    <option key={candidate.id} value={candidate.id}>{candidate.name}</option>
                  ))}
                </select>
              </div>
              <div className="usage-cost-value">{all.total > 0 ? formatCost(costFor(all, model)) : '$0.00'}</div>
              <p className="usage-note">is what these tokens would have cost on {model.name}. On your Mac they cost nothing beyond electricity.</p>
              <p className="usage-note">Anthropic list prices, October 2026: ${model.input} input, ${model.cachedInput} cached input, ${model.output} output per million tokens. Tokenizers differ, so this is an estimate.</p>
            </div>
          </div>

          <div className="usage-cards usage-breakdowns">
            <div className="usage-card">
              <Breakdown title="By Model" items={all.models} />
            </div>
            <div className="usage-card">
              <Breakdown title="By App or Agent" items={all.clients} />
            </div>
          </div>
        </>
      )}
    </div>
  );
}
