import { useQuery } from '@tanstack/react-query'
import { Link } from 'react-router'
import { api, formatEpochMs, formatNumber, formatUsd, isoDate } from '../api'
import { Badge, ErrorNote, PageTitle, StatTile, Table } from '../components'
import type { DailyActivity, ModelInfoEntry, Readiness, SpendLogsPage } from '../types'

function tolerant<T>(promise: Promise<T>): Promise<T | null> {
  return promise.catch(() => null)
}

export function OverviewPage() {
  const today = isoDate(new Date())
  const readiness = useQuery({ queryKey: ['readiness'], queryFn: () => api<Readiness>('/health/readiness') })
  const models = useQuery({ queryKey: ['model-info'], queryFn: () => api<{ data: ModelInfoEntry[] }>('/model/info') })
  const activity = useQuery({
    queryKey: ['activity-today', today],
    queryFn: () => tolerant(api<DailyActivity>('/user/daily/activity', { query: { start_date: today, end_date: today } })),
  })
  const spend = useQuery({ queryKey: ['global-spend'], queryFn: () => tolerant(api<{ spend: number; max_budget: number | null }>('/global/spend')) })
  const logs = useQuery({
    queryKey: ['recent-logs'],
    queryFn: () => tolerant(api<SpendLogsPage>('/spend/logs/ui', { query: { page_size: 10 } })),
  })

  const todayMetrics = activity.data?.results?.[0]?.metrics
  const deployments = models.data?.data ?? []
  const healthy = deployments.filter((d) => d.model_info.health?.status === 'healthy').length
  const unhealthy = deployments.filter((d) => d.model_info.health?.status === 'unhealthy').length
  const dbEnabled = readiness.data?.db === 'connected'

  return (
    <div>
      <PageTitle title="Overview" />
      <ErrorNote error={models.error} />
      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <StatTile
          label="Gateway"
          value={<Badge tone={readiness.data?.status === 'healthy' ? 'good' : 'bad'}>{readiness.data?.status ?? '…'}</Badge>}
          hint={readiness.data ? `v${readiness.data.version} · up ${Math.round(readiness.data.uptime_s / 60)} min · db ${readiness.data.db}` : undefined}
        />
        <StatTile label="Deployments" value={deployments.length} hint={`${healthy} healthy · ${unhealthy} unhealthy · ${deployments.length - healthy - unhealthy} unchecked`} />
        <StatTile label="Requests today" value={todayMetrics ? formatNumber(todayMetrics.api_requests) : dbEnabled ? '0' : '—'} hint={todayMetrics ? `${todayMetrics.failed_requests} failed` : 'needs a database'} />
        <StatTile label="Spend today" value={todayMetrics ? formatUsd(todayMetrics.spend) : dbEnabled ? '$0.0000' : '—'} hint={spend.data ? `all time ${formatUsd(spend.data.spend, 2)}` : undefined} />
      </div>

      <h2 className="mt-8 mb-3 text-lg font-semibold">Recent requests</h2>
      {!dbEnabled ? (
        <p className="text-sm text-slate-500">Spend logs need a database (`DATABASE_URL`). The gateway is running with the master key only.</p>
      ) : (
        <Table
          rows={logs.data?.data ?? []}
          rowKey={(row) => row.request_id}
          columns={[
            { key: 'time', header: 'Time', render: (row) => formatEpochMs(row.start_time) },
            { key: 'model', header: 'Model', render: (row) => row.model },
            { key: 'kind', header: 'Type', render: (row) => row.call_type },
            { key: 'key', header: 'Key', render: (row) => row.key_alias ?? row.api_key.slice(0, 8) },
            { key: 'status', header: 'Status', render: (row) => <Badge tone={row.status === 'success' ? 'good' : 'bad'}>{row.status}</Badge> },
            { key: 'tokens', header: 'Tokens', render: (row) => formatNumber(row.total_tokens) },
            { key: 'spend', header: 'Spend', render: (row) => formatUsd(row.spend) },
          ]}
          empty="No requests logged yet."
        />
      )}
      <p className="mt-3 text-sm text-slate-500">
        <Link to="/logs" className="underline">
          All logs
        </Link>
      </p>
    </div>
  )
}
