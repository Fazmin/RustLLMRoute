import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Bar, BarChart, CartesianGrid, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts'
import { api, formatNumber, formatUsd, isoDate } from '../api'
import { Button, ErrorNote, Field, Loading, PageTitle, Select, StatTile, Table } from '../components'
import type { DailyActivity, Metrics } from '../types'

interface BreakdownRow {
  name: string
  metrics: Metrics
}

export function UsagePage() {
  const [start, setStart] = useState(isoDate(new Date(Date.now() - 29 * 86400_000)))
  const [end, setEnd] = useState(isoDate(new Date()))
  const [groupBy, setGroupBy] = useState('model')
  const [applied, setApplied] = useState({ start, end, groupBy })
  const activity = useQuery({
    queryKey: ['activity', applied],
    queryFn: () => api<DailyActivity>('/user/daily/activity', { query: { start_date: applied.start, end_date: applied.end, group_by: applied.groupBy } }),
  })

  const results = activity.data?.results ?? []
  const chart = results.map((day) => ({ date: day.date.slice(5), spend: Number(day.metrics.spend.toFixed(4)), requests: day.metrics.api_requests }))
  const totals = results.reduce(
    (acc, day) => ({
      spend: acc.spend + day.metrics.spend,
      requests: acc.requests + day.metrics.api_requests,
      tokens: acc.tokens + day.metrics.total_tokens,
      failed: acc.failed + day.metrics.failed_requests,
    }),
    { spend: 0, requests: 0, tokens: 0, failed: 0 },
  )
  const breakdown = new Map<string, Metrics>()
  for (const day of results) {
    for (const dimension of Object.values(day.breakdown ?? {})) {
      for (const [name, entry] of Object.entries(dimension)) {
        const current = breakdown.get(name)
        const metrics = entry.metrics
        if (!current) {
          breakdown.set(name, { ...metrics })
        } else {
          current.spend += metrics.spend
          current.api_requests += metrics.api_requests
          current.total_tokens += metrics.total_tokens
          current.prompt_tokens += metrics.prompt_tokens
          current.completion_tokens += metrics.completion_tokens
          current.failed_requests += metrics.failed_requests
          current.successful_requests += metrics.successful_requests
        }
      }
    }
  }
  const rows: BreakdownRow[] = [...breakdown.entries()].map(([name, metrics]) => ({ name, metrics })).sort((a, b) => b.metrics.spend - a.metrics.spend)

  return (
    <div>
      <PageTitle title="Usage" />
      <ErrorNote error={activity.error} />
      <div className="mb-4 grid grid-cols-2 gap-3 md:grid-cols-5">
        <Field label="From" type="date" value={start} onChange={setStart} />
        <Field label="To" type="date" value={end} onChange={setEnd} />
        <Select
          label="Group by"
          value={groupBy}
          onChange={setGroupBy}
          options={[
            { value: 'model', label: 'Model' },
            { value: 'api_key', label: 'Key' },
            { value: 'team', label: 'Team' },
            { value: 'user', label: 'User' },
            { value: 'provider', label: 'Provider' },
          ]}
        />
        <div className="flex items-end">
          <Button onClick={() => setApplied({ start, end, groupBy })}>Apply</Button>
        </div>
      </div>
      {activity.isLoading ? (
        <Loading />
      ) : (
        <>
          <div className="mb-6 grid grid-cols-2 gap-4 lg:grid-cols-4">
            <StatTile label="Spend" value={formatUsd(totals.spend, 2)} />
            <StatTile label="Requests" value={formatNumber(totals.requests)} hint={`${formatNumber(totals.failed)} failed`} />
            <StatTile label="Tokens" value={formatNumber(totals.tokens)} />
            <StatTile label="Days" value={results.length} />
          </div>
          <div className="mb-6 h-64 rounded-lg border border-slate-200 bg-white p-3">
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={chart}>
                <CartesianGrid strokeDasharray="3 3" stroke="#e2e8f0" />
                <XAxis dataKey="date" fontSize={12} />
                <YAxis fontSize={12} />
                <Tooltip />
                <Bar dataKey="spend" name="Spend (USD)" fill="#0f172a" />
              </BarChart>
            </ResponsiveContainer>
          </div>
          <Table
            rows={rows}
            rowKey={(row) => row.name}
            columns={[
              { key: 'name', header: applied.groupBy, render: (row) => row.name },
              { key: 'spend', header: 'Spend', render: (row) => formatUsd(row.metrics.spend) },
              { key: 'requests', header: 'Requests', render: (row) => formatNumber(row.metrics.api_requests) },
              { key: 'tokens', header: 'Tokens', render: (row) => formatNumber(row.metrics.total_tokens) },
              { key: 'failed', header: 'Failed', render: (row) => formatNumber(row.metrics.failed_requests) },
            ]}
            empty="No usage in this range."
          />
        </>
      )}
    </div>
  )
}
