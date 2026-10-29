import { useState } from 'react'
import { useNavigate } from 'react-router'
import { api, setToken } from '../api'
import { Button, ErrorNote, Field } from '../components'

export function LoginPage() {
  const navigate = useNavigate()
  const [username, setUsername] = useState('admin')
  const [password, setPassword] = useState('')
  const [masterKey, setMasterKey] = useState('')
  const [error, setError] = useState<unknown>(null)
  const [busy, setBusy] = useState(false)

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    setBusy(true)
    setError(null)
    try {
      if (masterKey.trim()) {
        setToken(masterKey.trim())
        await api('/model/info', { redirectOnUnauthorized: false })
      } else {
        const result = await api<{ token: string }>('/login', { json: { username, password }, redirectOnUnauthorized: false })
        setToken(result.token)
      }
      navigate('/')
    } catch (failure) {
      setError(failure)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center p-6">
      <form onSubmit={submit} className="w-full max-w-sm space-y-4 rounded-lg border border-slate-200 bg-white p-6 shadow-sm">
        <div>
          <h1 className="text-xl font-semibold">RustLLMroute</h1>
          <p className="text-sm text-slate-500">Sign in to the admin dashboard.</p>
        </div>
        <ErrorNote error={error} />
        <Field label="Username" value={username} onChange={setUsername} />
        <Field label="Password" type="password" value={password} onChange={setPassword} hint="Defaults to the master key unless UI_PASSWORD is set." />
        <div className="text-center text-xs text-slate-400">or</div>
        <Field label="Master key" type="password" value={masterKey} onChange={setMasterKey} placeholder="sk-…" hint="Used as a bearer token for this browser session only." />
        <Button type="submit" disabled={busy}>
          {busy ? 'Signing in…' : 'Sign in'}
        </Button>
      </form>
    </div>
  )
}
