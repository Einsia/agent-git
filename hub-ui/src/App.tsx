import { lazy, Suspense } from "react"
import { BrowserRouter, Link, Route, Routes } from "react-router-dom"

import { Layout } from "@/components/Layout"
import { SessionProvider } from "@/lib/session"
import { Home } from "@/pages/Home"
import { Login } from "@/pages/Login"
import { Register } from "@/pages/Register"
import { ResetPassword } from "@/pages/ResetPassword"
import { Orgs } from "@/pages/Orgs"
import { OrgDetail } from "@/pages/OrgDetail"
import { Repos } from "@/pages/Repos"
import { NewAgent } from "@/pages/NewAgent"
import { Agent } from "@/pages/Agent"
import { Mrs } from "@/pages/Mrs"
import { Settings } from "@/pages/Settings"
import { Tokens } from "@/pages/Tokens"
import { Account } from "@/pages/Account"
import { Admin } from "@/pages/Admin"
import { Audit } from "@/pages/Audit"

// CODE-SPLIT: the heavy pages are lazy-loaded so their weight stays OUT of the initial bundle. Session
// pulls in react-markdown + remark-gfm + @tanstack/react-virtual; Diff and MrDetail also render markdown.
// Loading them on demand keeps app.js (the entry) small — a visitor on Home/Login never downloads the
// markdown/virtualizer code. Routing behavior is unchanged; a small Suspense fallback covers the fetch.
// (These pages export named components, re-exported as `default` for React.lazy.)
const Session = lazy(() => import("@/pages/Session").then((m) => ({ default: m.Session })))
const Diff = lazy(() => import("@/pages/Diff").then((m) => ({ default: m.Diff })))
const MrDetail = lazy(() => import("@/pages/MrDetail").then((m) => ({ default: m.MrDetail })))

export default function App() {
  return (
    <BrowserRouter>
      <SessionProvider>
        <Layout>
          <Suspense fallback={<p className="py-6 text-muted-foreground">Loading…</p>}>
            <Routes>
              <Route path="/" element={<Home />} />
              <Route path="/login" element={<Login />} />
              <Route path="/register" element={<Register />} />
              <Route path="/reset-password" element={<ResetPassword />} />
              <Route path="/new" element={<NewAgent />} />
              <Route path="/orgs" element={<Orgs />} />
              <Route path="/orgs/:name" element={<OrgDetail />} />
              <Route path="/repos" element={<Repos />} />
              <Route path="/tokens" element={<Tokens />} />
              <Route path="/account" element={<Account />} />
              <Route path="/admin" element={<Admin />} />
              <Route path="/audit" element={<Audit />} />
              <Route path="/agent/:owner/:name" element={<Agent />} />
              <Route path="/agent/:owner/:name/mrs" element={<Mrs />} />
              <Route path="/agent/:owner/:name/mrs/:id" element={<MrDetail />} />
              <Route path="/agent/:owner/:name/settings" element={<Settings />} />
              <Route path="/agent/:owner/:name/session/:id" element={<Session />} />
              <Route path="/agent/:owner/:name/session/:id/diff" element={<Diff />} />
              <Route path="*" element={<NotFound />} />
            </Routes>
          </Suspense>
        </Layout>
      </SessionProvider>
    </BrowserRouter>
  )
}

function NotFound() {
  return (
    <div className="readout rounded-lg border px-6 py-12 text-center">
      <p className="eyebrow">404</p>
      <p className="mb-1 mt-2 font-semibold">No such page</p>
      <p className="mx-auto mb-5 max-w-[46ch] text-sm text-muted-foreground">
        Nothing is routed here. A private agent you can't read looks the same as one that
        doesn't exist — that's deliberate.
      </p>
      <Link to="/" className="font-mono text-sm text-primary hover:underline">
        ← all agents
      </Link>
    </div>
  )
}
