import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { api, formatEpochMs, formatNumber, formatUsd, isoDate } from '../api'
import { Badge, Button, ErrorNote, Field, JsonView, Loading, Modal, PageTitle, Pager, Select, Table } from '../components'
import type { SpendLog, SpendLogsPage } from '../types'

export function LogsPage() {
  const [page, setPage] = useState(1)
  const [apiKey, setApiKey] = useState('')
  const [model, setModel] = useState('')
  const [status, setStatus] = useState('')
  const [start, setStart] = useState(isoDate(new Date(Date.now() - 6 * 86400_000)))
  const [end, setEnd] = useState(isoDate(new Date()))
  const [applied, setApplied] = useState({ apiKey: '', model: '', status: '', start, end })
  const [selected, setSelected] = useState<SpendLog | null>(null)

  const logs = useQuery({
    queryKey: ['logs', page, applied],
    queryFn: () =>
      api<SpendLogsPage>('/spend/logs/ui', {
        query: {
          page,
          page_size: 50,
          api_key: applied.apiKey || undefined,
          model: applied.model || undefined,
          status_filter: applied.status || undefined,
          start_date: applied.start || undefined,
          end_date: applied.end || undefined,
        },
      }),
  })

  return (
    <div>
      <PageTitle title="Request logs" />
      <ErrorNote error={logs.error} />
      <div className="mb-4 grid grid-cols-2 gap-3 md:grid-cols-6">
        <Field label="Key (sk-… or hash)" value={apiKey} onChange={setApiKey} />
        <Field label="Model" value={model} onChange={setModel} />
        <Select
          label="Status"
          value={status}
          onChange={setStatus}
          options={[
            { value: '', label: 'Any' },
            { value: 'success', label: 'Success' },
            { value: 'failure', label: 'Failure' },
          ]}
        />
        <Field label="From" type="date" value={start} onChange={setStart} />
        <Field label="To" type="date" value={end} onChange={setEnd} />
        <div className="flex items-end">
          <Button
            onClick={() => {
              setPage(1)
              setApplied({ apiKey, model, status, start, end })
            }}
          >
            Apply
          </Button>
        </div>
      </div>
      {logs.isLoading ? (
        <Loading />
      ) : (
        <Table
          rows={logs.data?.data ?? []}
          rowKey={(row) => row.request_id}
          onRowClick={setSelected}
          columns={[
            { key: 'time', header: 'Time', render: (row) => formatEpochMs(row.start_time) },
            { key: 'model', header: 'Model', render: (row) => row.model },
            { key: 'deployment', header: 'Deployment', render: (row) => <span className="font-mono text-xs">{row.model_id ?? '—'}</span> },
            { key: 'type', header: 'Type', render: (row) => row.call_type },
            { key: 'key', header: 'Key', render: (row) => row.key_alias ?? row.api_key.slice(0, 10) },
            { key: 'status', header: 'Status', render: (row) => <Badge tone={row.status === 'success' ? 'good' : 'bad'}>{row.status}</Badge> },
            { key: 'tokens', header: 'Tokens', render: (row) => `${formatNumber(row.prompt_tokens)} / ${formatNumber(row.completion_tokens)}` },
            { key: 'spend', header: 'Spend', render: (row) => formatUsd(row.spend) },
            { key: 'duration', header: 'Duration', render: (row) => (row.request_duration_ms !== null ? `${row.request_duration_ms} ms` : '—') },
          ]}
          empty="No requests match these filters."
        />
      )}
      <Pager page={logs.data?.page ?? 1} totalPages={logs.data?.total_pages ?? 1} onChange={setPage} />
      {selected ? (
        <Modal title={`Request ${selected.request_id}`} onClose={() => setSelected(null)}>
          <JsonView value={selected} />
        </Modal>
      ) : null}
    </div>
  )
}
