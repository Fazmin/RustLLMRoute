import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createBrowserRouter, RouterProvider } from 'react-router'
import './index.css'
import { Layout } from './components'
import { LoginPage } from './pages/Login'
import { OverviewPage } from './pages/Overview'
import { KeysPage } from './pages/Keys'
import { TeamsPage } from './pages/Teams'
import { UsersPage } from './pages/Users'
import { ModelsPage } from './pages/Models'
import { LogsPage } from './pages/Logs'
import { UsagePage } from './pages/Usage'
import { SettingsPage } from './pages/Settings'

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false, staleTime: 10_000 } },
})

const router = createBrowserRouter(
  [
    { path: '/login', element: <LoginPage /> },
    {
      path: '/',
      element: <Layout />,
      children: [
        { index: true, element: <OverviewPage /> },
        { path: 'keys', element: <KeysPage /> },
        { path: 'teams', element: <TeamsPage /> },
        { path: 'users', element: <UsersPage /> },
        { path: 'models', element: <ModelsPage /> },
        { path: 'logs', element: <LogsPage /> },
        { path: 'usage', element: <UsagePage /> },
        { path: 'settings', element: <SettingsPage /> },
      ],
    },
  ],
  { basename: '/ui' },
)

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <RouterProvider router={router} />
    </QueryClientProvider>
  </StrictMode>,
)
