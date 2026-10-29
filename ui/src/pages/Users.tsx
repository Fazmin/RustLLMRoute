import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, formatUsd } from '../api'
import { Button, ErrorNote, Field, Loading, Modal, PageTitle, Select, Table, numberOrUndefined } from '../components'
import type { UserRow } from '../types'

export function UsersPage() {
  const queryClient = useQueryClient()
  const [showCreate, setShowCreate] = useState(false)
  const [error, setError] = useState<unknown>(null)
  const list = useQuery({ queryKey: ['users'], queryFn: () => api<{ users: UserRow[] }>('/user/list', { query: { size: 200 } }) })
  const action = useMutation({
    mutationFn: ({ path, body }: { path: string; body: unknown }) => api(path, { json: body }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['users'] }),
    onError: setError,
  })
  const [userId, setUserId] = useState('')
  const [email, setEmail] = useState('')
  const [role, setRole] = useState('internal_user')
  const [maxBudget, setMaxBudget] = useState('')

  return (
    <div>
      <PageTitle title="Users" actions={<Button onClick={() => setShowCreate(true)}>Create user</Button>} />
      <ErrorNote error={error ?? list.error} />
      {list.isLoading ? (
        <Loading />
      ) : (
        <Table
          rows={list.data?.users ?? []}
          rowKey={(row) => row.user_id}
          columns={[
            { key: 'id', header: 'User', render: (row) => <span className="font-mono text-xs">{row.user_id}</span> },
            { key: 'email', header: 'Email', render: (row) => row.user_email ?? '—' },
            { key: 'role', header: 'Role', render: (row) => row.user_role },
            { key: 'spend', header: 'Spend', render: (row) => `${formatUsd(row.spend)} / ${row.max_budget === null ? '∞' : formatUsd(row.max_budget, 2)}` },
            { key: 'models', header: 'Models', render: (row) => (row.models.length ? row.models.join(', ') : 'all') },
            {
              key: 'actions',
              header: '',
              render: (row) => (
                <Button
                  kind="danger"
                  onClick={() => {
                    if (window.confirm(`Delete user ${row.user_id}?`)) action.mutate({ path: '/user/delete', body: { user_ids: [row.user_id] } })
                  }}
                >
                  Delete
                </Button>
              ),
            },
          ]}
          empty="No users yet."
        />
      )}
      {showCreate ? (
        <Modal title="Create a user" onClose={() => setShowCreate(false)}>
          <div className="space-y-3">
            <Field label="User id" value={userId} onChange={setUserId} placeholder="alice" hint="Optional; generated when empty" />
            <Field label="Email" value={email} onChange={setEmail} />
            <Select
              label="Role"
              value={role}
              onChange={setRole}
              options={[
                { value: 'internal_user', label: 'Internal user' },
                { value: 'internal_user_viewer', label: 'Viewer' },
                { value: 'proxy_admin', label: 'Admin' },
              ]}
            />
            <Field label="Max budget (USD)" value={maxBudget} onChange={setMaxBudget} />
          </div>
          <div className="mt-4 flex justify-end gap-2">
            <Button kind="secondary" onClick={() => setShowCreate(false)}>
              Cancel
            </Button>
            <Button
              onClick={() => {
                setShowCreate(false)
                action.mutate({
                  path: '/user/new',
                  body: { user_id: userId || undefined, user_email: email || undefined, user_role: role, max_budget: numberOrUndefined(maxBudget), auto_create_key: false },
                })
              }}
            >
              Create
            </Button>
          </div>
        </Modal>
      ) : null}
    </div>
  )
}
