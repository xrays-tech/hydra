// Package hydra provides a small Go SDK for the Hydra tenant self-service
// auth-cache invalidation endpoint.
//
// The client accepts one or more Hydra **data-plane** node base URLs (the
// address that serves `/v1/*`, default port 8080 — not the admin port 8081).
//
// The invalidation endpoint is answered **locally by every data-plane node**:
// it clears that node's cache synchronously and fans the invalidation out over
// the shared bus, so it does NOT need to reach the cluster leader (design
// `dev-docs/design-tenant-api.md` §6.4 decision A-1). Leader preference via the
// `/healthz/leader` probe is therefore a legacy optimization, never a
// requirement.
//
// Before each invalidation the client still probes `/healthz/leader` and tries
// the nodes that report themselves leader first. That probe is an **admin-port**
// route: a node list pointing at data-plane ports gets a 404 from it, which is
// treated as "alive, not the leader" so the request is sent directly. If the
// chosen node fails, the client automatically rotates to the next available
// node and temporarily removes the dead node from the active pool. A background
// rechecker periodically probes removed nodes and adds them back once they
// become reachable again.
//
// # The endpoint's three-state contract
//
// A 2xx is NOT the same as "done". Per `dev-docs/tenant-api-integration.md` §5.2
// the fleet state in the response body is what the caller must branch on, and
// [InvalidateTenantAuthCacheResult] surfaces it verbatim:
//
//	HTTP  fleet.state    meaning                                  caller must
//	200   applied        every live node applied it              done
//	200   single_node    this node IS the whole data plane       done
//	202   pending        published, not confirmed everywhere     retry (idempotent) or report lagging
//	503   unavailable    the cluster was NOT notified            retry / escalate, quoting TraceID
//
// Two consequences worth stating because the old behaviour got both wrong:
//
//   - `202 pending` is a *retryable* outcome, never silent success. The wrapper
//     methods return an error for it ([*InvalidatePendingError]); callers that
//     want to branch on it themselves should call
//     [Client.InvalidateTenantAuthCacheResult].
//   - `503 unavailable` is NOT a node failure. The node answered — it is alive
//     and its own cache was cleared — so it is neither quarantined nor rotated
//     away from, and the "the cluster was not notified" signal is preserved as
//     [*InvalidateUnavailableError]. Only transport failures and non-fleet-aware
//     server errors still trigger failover.
package hydra

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Default values used when a Config field is left zero.
const (
	DefaultProbeTimeout    = 2 * time.Second
	DefaultRequestTimeout  = 10 * time.Second
	DefaultRecheckInterval = 30 * time.Second
)

// Query-parameter bounds, mirroring the server's own validation
// (`crates/hydra-server/src/tenant_api/handlers.rs`: `MAX_CONVERGE_MS = 60_000`
// and `wait` ∈ {converged, none}). Values outside these are rejected client-side
// so a typo fails here rather than as a remote `400 invalid_wait` /
// `400 invalid_timeout_ms`.
const (
	// MinTimeoutMS is the smallest accepted `timeout_ms`.
	MinTimeoutMS = 1
	// MaxTimeoutMS is the largest accepted `timeout_ms`.
	MaxTimeoutMS = 60_000
)

// WaitMode is the `wait` query parameter (tenant API §5.2).
type WaitMode string

const (
	// WaitConverged blocks until every live node has applied the event. It is
	// the server's own default, so setting it is equivalent to leaving WaitMode
	// empty — except that it is then sent explicitly.
	WaitConverged WaitMode = "converged"
	// WaitNone publishes and answers `202` immediately without waiting, which
	// is the bulk/script mode. The response's EventID is for later
	// reconciliation.
	WaitNone WaitMode = "none"
)

// valid reports whether m is a value the server accepts.
func (m WaitMode) valid() bool {
	return m == WaitConverged || m == WaitNone
}

// Config configures a Hydra cluster client.
type Config struct {
	// Token is the tenant self-service access token. It is sent as
	// "Authorization: Bearer <token>" on every invalidation request.
	Token string

	// Nodes is a list of Hydra **data-plane** node base URLs — the same
	// address clients use for /v1/*, default port 8080, e.g.
	// "http://127.0.0.1:8080". It is NOT the admin port 8081. At least one
	// node is required.
	Nodes []string

	// HTTPClient is used for all HTTP requests. When nil, http.DefaultClient
	// is used.
	HTTPClient *http.Client

	// ProbeTimeout limits each /healthz/leader probe. Default 2s.
	ProbeTimeout time.Duration

	// RequestTimeout limits each invalidation request. Default 10s.
	RequestTimeout time.Duration

	// RecheckInterval controls how often removed nodes are probed for
	// re-addition. Default 30s.
	RecheckInterval time.Duration

	// DisableBackgroundRecheck disables the automatic periodic rechecking of
	// removed nodes. Call ProbeRemovedNodes manually in that case.
	DisableBackgroundRecheck bool

	// WaitMode is the `wait` query parameter sent on every invalidation
	// request. Accepts WaitConverged or WaitNone; any other non-zero value is
	// rejected by New rather than silently ignored.
	//
	// Leaving it empty (the default) sends NO `wait` parameter at all, so the
	// server applies its own default, which is `converged` — i.e. the request
	// blocks for up to TimeoutMS (or the server's configured budget, default
	// 2000 ms) waiting for the whole fleet to confirm.
	WaitMode WaitMode

	// TimeoutMS is the `timeout_ms` query parameter: this request's budget in
	// milliseconds for waiting on fleet confirmation. Must be in
	// [MinTimeoutMS, MaxTimeoutMS] (1..60000) when non-zero.
	//
	// Leaving it zero (the default) sends NO `timeout_ms` parameter, so the
	// server uses its configured budget, which defaults to 2000 ms. Note the
	// difference between "unset" (server default) and WaitNone (do not wait at
	// all): the latter is a statement about behaviour, the former is not.
	TimeoutMS int
}

// Client is a concurrency-safe Hydra tenant SDK client with automatic leader
// discovery and node failover.
type Client struct {
	token           string
	httpClient      *http.Client
	probeTimeout    time.Duration
	requestTimeout  time.Duration
	recheckInterval time.Duration
	waitMode        WaitMode
	timeoutMS       int

	mu      sync.RWMutex
	active  []string // reachable candidate nodes
	removed []string // temporarily quarantined nodes

	ctx    context.Context
	cancel context.CancelFunc
	wg     sync.WaitGroup
	once   sync.Once
}

// HTTPError is returned when the server responds with a non-2xx status that is
// NOT one of the documented invalidate outcomes.
//
// Note that `503 unavailable` on the invalidate endpoint does NOT produce one:
// that answer carries a normal fleet body, so it is reported as a result (or as
// [*InvalidateUnavailableError] by the wrappers). A `503 not_ready` or a `500`
// still does.
type HTTPError struct {
	Method     string
	URL        string
	Status     int
	StatusText string
	Body       string
	// TraceID is the `X-Hydra-Trace-Id` response header, carried on the error
	// itself because the tenant contract (§4.2) tells callers to quote it when
	// reporting a problem — and an HTTP error is a problem.
	TraceID string
}

func (e *HTTPError) Error() string {
	body := strings.TrimSpace(e.Body)
	if len(body) > 300 {
		body = body[:300] + "..."
	}
	trace := ""
	if e.TraceID != "" {
		trace = " (X-Hydra-Trace-Id: " + e.TraceID + ")"
	}
	if body != "" {
		return fmt.Sprintf("%s %s: unexpected HTTP %d %s: %s%s", e.Method, e.URL, e.Status, e.StatusText, body, trace)
	}
	return fmt.Sprintf("%s %s: unexpected HTTP %d %s%s", e.Method, e.URL, e.Status, e.StatusText, trace)
}

// New creates a Client from cfg and starts the automatic node recheck loop.
// Use Close to stop the background goroutine.
func New(cfg Config) (*Client, error) {
	if strings.TrimSpace(cfg.Token) == "" {
		return nil, errors.New("hydra: token is required")
	}
	if len(cfg.Nodes) == 0 {
		return nil, errors.New("hydra: at least one node is required")
	}

	httpClient := cfg.HTTPClient
	if httpClient == nil {
		httpClient = http.DefaultClient
	}
	probeTimeout := cfg.ProbeTimeout
	if probeTimeout <= 0 {
		probeTimeout = DefaultProbeTimeout
	}
	requestTimeout := cfg.RequestTimeout
	if requestTimeout <= 0 {
		requestTimeout = DefaultRequestTimeout
	}
	recheckInterval := cfg.RecheckInterval
	if recheckInterval <= 0 {
		recheckInterval = DefaultRecheckInterval
	}

	// The wait/timeout_ms pair is validated rather than clamped or ignored: the
	// server treats a bad value as a hard `400`, and silently substituting a
	// default would leave the caller believing it had asked for (or skipped) a
	// wait it never got.
	if cfg.WaitMode != "" && !cfg.WaitMode.valid() {
		return nil, fmt.Errorf("hydra: invalid WaitMode %q (want %q or %q)", cfg.WaitMode, WaitConverged, WaitNone)
	}
	if cfg.TimeoutMS != 0 && (cfg.TimeoutMS < MinTimeoutMS || cfg.TimeoutMS > MaxTimeoutMS) {
		return nil, fmt.Errorf("hydra: invalid TimeoutMS %d (want %d..%d, or 0 to use the server default)", cfg.TimeoutMS, MinTimeoutMS, MaxTimeoutMS)
	}

	seen := make(map[string]struct{}, len(cfg.Nodes))
	active := make([]string, 0, len(cfg.Nodes))
	for _, node := range cfg.Nodes {
		node = strings.TrimSpace(node)
		if node == "" {
			continue
		}
		node = strings.TrimRight(node, "/")
		u, err := url.Parse(node)
		if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
			return nil, fmt.Errorf("hydra: invalid node URL %q", node)
		}
		if _, ok := seen[node]; ok {
			continue
		}
		seen[node] = struct{}{}
		active = append(active, node)
	}
	if len(active) == 0 {
		return nil, errors.New("hydra: no valid node URLs")
	}

	ctx, cancel := context.WithCancel(context.Background())
	c := &Client{
		token:           strings.TrimSpace(cfg.Token),
		httpClient:      httpClient,
		probeTimeout:    probeTimeout,
		requestTimeout:  requestTimeout,
		recheckInterval: recheckInterval,
		waitMode:        cfg.WaitMode,
		timeoutMS:       cfg.TimeoutMS,
		active:          active,
		ctx:             ctx,
		cancel:          cancel,
	}

	if !cfg.DisableBackgroundRecheck {
		c.wg.Add(1)
		go c.recheckLoop()
	}
	return c, nil
}

// NewClient is an alias for New.
func NewClient(cfg Config) (*Client, error) {
	return New(cfg)
}

// NewWithToken creates a client with the required tenant access token and
// cluster node base URLs. It is a convenience wrapper around New.
func NewWithToken(token string, nodes []string) (*Client, error) {
	return New(Config{Token: token, Nodes: nodes})
}

// Close stops the background rechecker and waits for it to exit.
func (c *Client) Close() error {
	c.once.Do(func() {
		if c.cancel != nil {
			c.cancel()
		}
		c.wg.Wait()
	})
	return nil
}

// Nodes returns a snapshot of currently active node base URLs.
func (c *Client) Nodes() []string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return append([]string(nil), c.active...)
}

// RemovedNodes returns a snapshot of currently quarantined node base URLs.
func (c *Client) RemovedNodes() []string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return append([]string(nil), c.removed...)
}

// FleetState is the server's `fleet.state` token. The four values are the
// server's own (`crates/hydra-server/src/cluster/events.rs`,
// `FleetReport.state`) and are exposed verbatim — there is deliberately no
// mapping layer, because a mapping is a second owner of the contract and two
// owners always eventually disagree.
type FleetState string

const (
	// FleetApplied — every live node has applied the invalidation. HTTP 200.
	FleetApplied FleetState = "applied"
	// FleetSingleNode — this node IS the whole data plane, so its own clear is
	// the whole answer. HTTP 200.
	FleetSingleNode FleetState = "single_node"
	// FleetPending — published but not confirmed everywhere. HTTP 202. NOT an
	// error, and not yet "done": retry (the call is idempotent) or hand
	// `lagging` to the operator.
	FleetPending FleetState = "pending"
	// FleetUnavailable — an invalidation channel exists but did not answer, so
	// the cluster was NOT notified. HTTP 503. The node itself is alive.
	FleetUnavailable FleetState = "unavailable"
)

// Done reports whether this state means the invalidation is actually complete
// per the contract: only `applied` and `single_node` qualify. `pending` is in
// flight and `unavailable` never reached the fleet.
func (s FleetState) Done() bool {
	return s == FleetApplied || s == FleetSingleNode
}

// Retryable reports whether the caller should try again. `pending` and
// `unavailable` are both retryable — the call is idempotent (§4.5) — while a
// done state has nothing left to retry.
func (s FleetState) Retryable() bool {
	return s == FleetPending || s == FleetUnavailable
}

// InvalidateTenantAuthCacheResult is the full outcome of one invalidation.
//
// It is deliberately not a boolean: the endpoint has a three-state contract and
// every one of the three tells the caller to do something different.
type InvalidateTenantAuthCacheResult struct {
	// HTTPStatus is the status the winning node returned: 200 (applied /
	// single_node), 202 (pending) or 503 (unavailable).
	HTTPStatus int

	// State is the fleet state, verbatim from the response body. Empty only
	// when the body could not be decoded into the documented shape.
	State FleetState

	// Invalidated is how many entries THIS node removed from its own cache.
	// Not a fleet count. `Invalidated == 0 && Checked > 0` means those keys
	// were not cached on this node, which is not a failure.
	Invalidated int64

	// Checked is how many keys the request named (0 for a whole-tenant clear).
	Checked int64

	// Scope is "keys" or "tenant".
	Scope string

	// NodesApplied / NodesTotal are the confirmed / live node counts.
	NodesApplied int64
	NodesTotal   int64

	// Lagging names the nodes that did not confirm. Non-empty for `pending`,
	// and also for an `unavailable` whose convergence barrier (rather than
	// whose publish) failed.
	//
	// Treat it as "not confirmed", NOT as "proven behind". With WaitNone nobody
	// looked at the fleet and the server names every live node here
	// (`crates/hydra-server/src/cluster/events.rs`, the `budget == None` arm),
	// which the contract prose in `dev-docs/tenant-api-integration.md` §5.2
	// describes in the opposite way. The code wins.
	Lagging []string

	// EventID is this invalidation's id on the internal bus, for later
	// reconciliation. Empty when the server reported none.
	EventID string

	// WaitedMS is how long the server actually spent waiting for the fleet.
	WaitedMS int64

	// TraceID is the `X-Hydra-Trace-Id` response header — quote it in a
	// support request (§4.2). Empty when the node did not send one.
	TraceID string

	// Node is the base URL of the node that answered. TraceID plus Node is
	// what makes a partial failure diagnosable.
	Node string
}

// InvalidatePendingError reports HTTP 202 `pending`: the invalidation was
// published but the whole fleet has not confirmed it. It is NOT a failure of
// the request — it is an incomplete outcome, and the caller must retry or hand
// the lagging nodes to the operator.
//
// It exists as a distinct type so that callers who previously treated "no
// error" as "done" can no longer do so by accident, while still being able to
// distinguish it from a hard error with errors.As.
type InvalidatePendingError struct {
	Result InvalidateTenantAuthCacheResult
}

func (e *InvalidatePendingError) Error() string {
	return fmt.Sprintf(
		"hydra: auth cache invalidation is pending, not complete: node %s answered HTTP %d fleet.state=%q (%d/%d nodes applied, waited %dms, event_id %q, trace_id %q)",
		e.Result.Node, e.Result.HTTPStatus, e.Result.State,
		e.Result.NodesApplied, e.Result.NodesTotal, e.Result.WaitedMS,
		e.Result.EventID, e.Result.TraceID,
	)
}

// InvalidateUnavailableError reports HTTP 503 `unavailable`: an invalidation
// channel exists on that node but could not do its job, so THE CLUSTER WAS NOT
// NOTIFIED. The node itself answered, which is why this is not a node failure:
// the node is not quarantined and the client does NOT rotate away from it.
type InvalidateUnavailableError struct {
	Result InvalidateTenantAuthCacheResult
}

func (e *InvalidateUnavailableError) Error() string {
	return fmt.Sprintf(
		"hydra: auth cache invalidation is unavailable: node %s answered HTTP %d fleet.state=%q, so the cluster was NOT notified (waited %dms, event_id %q, trace_id %q); retry or escalate to an operator",
		e.Result.Node, e.Result.HTTPStatus, e.Result.State,
		e.Result.WaitedMS, e.Result.EventID, e.Result.TraceID,
	)
}

// InvalidateTenantAuthCacheResult invalidates the tenant's auth cache and
// returns the full, structured outcome.
//
// Prefer this method: it is the only one that can report the endpoint's middle
// state. Use it when you need to branch on the fleet state, to reconcile later
// with EventID, or to quote TraceID in a support request.
//
// It sends POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate to a
// data-plane node, preferring any node that reports itself leader, and fails
// over to other nodes when the chosen node is unreachable or returns a
// non-fleet-aware server error.
//
// Unlike the thin wrappers below it never turns a non-done outcome into an
// error: `202 pending` and `503 unavailable` come back as results. A non-nil
// error means the request itself could not produce a fleet report at all.
func (c *Client) InvalidateTenantAuthCacheResult(ctx context.Context, tenantID string, apiKeys []string) (InvalidateTenantAuthCacheResult, error) {
	var body map[string]any
	if len(apiKeys) > 0 {
		body = map[string]any{"api_keys": apiKeys}
	}
	return c.invalidate(ctx, tenantID, body)
}

// InvalidateTenantAuthCache invalidates the tenant's auth cache. It sends
// POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate to a data-plane node,
// preferring any node that reports itself leader, and fails over to other nodes
// when the chosen node is unreachable or returns a non-fleet-aware server error.
//
// It is a thin wrapper over [Client.InvalidateTenantAuthCacheResult] and returns
// an error unless the fleet state is `applied` or `single_node` — a 2xx is NOT
// sufficient. Use errors.As with [*InvalidatePendingError] and
// [*InvalidateUnavailableError] to tell the two non-done outcomes apart, or call
// [Client.InvalidateTenantAuthCacheResult] directly to branch on the state.
func (c *Client) InvalidateTenantAuthCache(ctx context.Context, tenantID string) error {
	return c.invalidateErr(ctx, tenantID, nil)
}

// InvalidateTenantAuthCacheKeys invalidates only the supplied api-keys for the
// tenant. When apiKeys is empty this is the same as InvalidateTenantAuthCache.
//
// Like InvalidateTenantAuthCache it is a thin wrapper: success means the fleet
// state was `applied` or `single_node`.
func (c *Client) InvalidateTenantAuthCacheKeys(ctx context.Context, tenantID string, apiKeys []string) error {
	if len(apiKeys) == 0 {
		return c.InvalidateTenantAuthCache(ctx, tenantID)
	}
	return c.invalidateErr(ctx, tenantID, map[string]any{"api_keys": apiKeys})
}

// invalidateErr adapts a structured result to the wrapper methods' error-only
// signature: success only for a state that actually means "done", with the two
// distinguishable non-done outcomes surfaced as their own error types.
func (c *Client) invalidateErr(ctx context.Context, tenantID string, body map[string]any) error {
	res, err := c.invalidate(ctx, tenantID, body)
	if err != nil {
		return err
	}
	if res.State.Done() {
		return nil
	}
	switch res.State {
	case FleetPending:
		return &InvalidatePendingError{Result: res}
	case FleetUnavailable:
		return &InvalidateUnavailableError{Result: res}
	}
	// A 2xx whose body did not carry a documented fleet state: we cannot claim
	// the invalidation is complete, so we must not report success.
	return fmt.Errorf(
		"hydra: auth cache invalidation returned HTTP %d with unrecognised fleet.state=%q (trace_id %q)",
		res.HTTPStatus, res.State, res.TraceID,
	)
}

// Invalidate is a short alias for InvalidateTenantAuthCache.
func (c *Client) Invalidate(ctx context.Context, tenantID string) error {
	return c.InvalidateTenantAuthCache(ctx, tenantID)
}

// InvalidateTenantCache is an alias for InvalidateTenantAuthCache.
func (c *Client) InvalidateTenantCache(ctx context.Context, tenantID string) error {
	return c.InvalidateTenantAuthCache(ctx, tenantID)
}

// InvalidateCache is another alias for InvalidateTenantAuthCache.
func (c *Client) InvalidateCache(ctx context.Context, tenantID string) error {
	return c.InvalidateTenantAuthCache(ctx, tenantID)
}

// ProbeRemovedNodes checks quarantined nodes and adds any reachable node back
// to the active pool. It is called automatically on every recheck interval.
func (c *Client) ProbeRemovedNodes(ctx context.Context) {
	c.mu.Lock()
	removed := append([]string(nil), c.removed...)
	c.mu.Unlock()

	if len(removed) == 0 {
		return
	}

	var restored []string
	for _, node := range removed {
		ok, _ := c.probeLeader(ctx, node)
		if ok {
			restored = append(restored, node)
		}
	}
	if len(restored) == 0 {
		return
	}

	c.mu.Lock()
	defer c.mu.Unlock()
	for _, node := range restored {
		if !contains(c.active, node) {
			c.active = append(c.active, node)
		}
		c.removed = removeString(c.removed, node)
	}
}

func (c *Client) recheckLoop() {
	defer c.wg.Done()
	ticker := time.NewTicker(c.recheckInterval)
	defer ticker.Stop()
	for {
		select {
		case <-c.ctx.Done():
			return
		case <-ticker.C:
			c.ProbeRemovedNodes(c.ctx)
		}
	}
}

func (c *Client) invalidate(ctx context.Context, tenantID string, body map[string]any) (InvalidateTenantAuthCacheResult, error) {
	if strings.TrimSpace(tenantID) == "" {
		return InvalidateTenantAuthCacheResult{}, errors.New("hydra: tenantID is required")
	}

	// Work on a snapshot so the rechecker can mutate the pools concurrently.
	c.mu.RLock()
	nodes := append([]string(nil), c.active...)
	c.mu.RUnlock()

	if len(nodes) == 0 {
		return InvalidateTenantAuthCacheResult{}, fmt.Errorf("hydra: no available nodes (removed: %s)", strings.Join(c.RemovedNodes(), ", "))
	}

	// Discover the active leader(s) first. Nodes that cannot be reached during
	// discovery are immediately moved to the removed pool.
	var leaders []string
	var alive []string
	seen := make(map[string]struct{}, len(nodes))
	for _, node := range nodes {
		ok, leader := c.probeLeader(ctx, node)
		if !ok {
			if ctx.Err() == nil {
				c.removeNode(node)
			}
			continue
		}
		if _, dup := seen[node]; dup {
			continue
		}
		seen[node] = struct{}{}
		alive = append(alive, node)
		if leader {
			leaders = append(leaders, node)
		}
	}

	// Invalidation is served LOCALLY by every data-plane node (design §6.4
	// decision A-1: it is deliberately NOT forwarded to the leader's admin
	// API), so leader preference is only a legacy ordering optimization — any
	// alive node can serve it. Leaders are tried first, then every other
	// reachable node.
	//
	// A node that answers the probe with anything other than 200 (e.g. 404
	// because the configured list points at data-plane ports, where
	// /healthz/leader does not exist) is "alive, not the leader": it stays in
	// the pool and is reached by the fallback below.
	attempts := append([]string(nil), leaders...)
	for _, node := range alive {
		if !contains(attempts, node) {
			attempts = append(attempts, node)
		}
	}

	if len(attempts) == 0 {
		return InvalidateTenantAuthCacheResult{}, fmt.Errorf("hydra: no reachable nodes (removed: %s)", strings.Join(c.RemovedNodes(), ", "))
	}

	var errs []error
	for _, node := range attempts {
		res, err := c.doInvalidate(ctx, node, tenantID, body)
		if err == nil {
			// A fleet-aware answer — including `503 unavailable` — is the
			// node's verdict about the CLUSTER, and the endpoint is answered
			// locally by whichever node received it, so another node would
			// answer the same question the same way. Return it rather than
			// rotating: rotating here was the defect that made
			// "the cluster was not notified" indistinguishable from "this node
			// is broken".
			//
			// 503 unavailable is deliberately NOT run through isNodeFailure:
			// the node answered, it is alive, and its own cache was cleared,
			// so it must not be quarantined.
			return res, nil
		}
		errs = append(errs, err)
		var httpErr *HTTPError
		if errors.As(err, &httpErr) && (httpErr.Status == http.StatusUnauthorized || httpErr.Status == http.StatusForbidden) {
			// Invalid/forbidden tenant token is a client-side error; rotating
			// to another node will not fix it.
			return res, err
		}
		if isNodeFailure(err) {
			c.removeNode(node)
		}
	}

	return InvalidateTenantAuthCacheResult{}, fmt.Errorf("hydra: all %d node(s) failed: %w", len(attempts), errors.Join(errs...))
}

func (c *Client) doInvalidate(ctx context.Context, node, tenantID string, body map[string]any) (InvalidateTenantAuthCacheResult, error) {
	endpoint := node + "/tenant/" + url.PathEscape(tenantID) + "/api/v1/auth/cache/invalidate"
	if q := c.queryParams(); q != "" {
		endpoint += "?" + q
	}
	var reader io.Reader
	if body != nil {
		payload, err := json.Marshal(body)
		if err != nil {
			return InvalidateTenantAuthCacheResult{}, err
		}
		reader = bytes.NewReader(payload)
	}
	reqCtx, cancel := context.WithTimeout(ctx, c.requestTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(reqCtx, http.MethodPost, endpoint, reader)
	if err != nil {
		return InvalidateTenantAuthCacheResult{}, err
	}
	req.Header.Set("Authorization", "Bearer "+c.token)
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return InvalidateTenantAuthCacheResult{}, fmt.Errorf("hydra: request to %s failed: %w", node, err)
	}
	defer resp.Body.Close()
	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))

	// The trace id is captured on BOTH paths: §4.2 tells the tenant to quote
	// `X-Hydra-Trace-Id` when reporting a problem, and a problem is exactly
	// what the error paths are.
	res := InvalidateTenantAuthCacheResult{
		HTTPStatus: resp.StatusCode,
		TraceID:    resp.Header.Get("X-Hydra-Trace-Id"),
		Node:       node,
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		// A 503 `unavailable` carries a NORMAL body — the documented fleet
		// structure with no `error.code` — so decode it before deciding this
		// is an HTTP failure. Reporting it as a bare HTTPError is what lost
		// the "the cluster was not notified" signal.
		if fleet, ok := parseFleetOutcome(respBody); ok {
			res.State = fleet.State
			res.Invalidated = fleet.Invalidated
			res.Checked = fleet.Checked
			res.Scope = fleet.Scope
			res.NodesApplied = fleet.NodesApplied
			res.NodesTotal = fleet.NodesTotal
			res.Lagging = fleet.Lagging
			res.EventID = fleet.EventID
			res.WaitedMS = fleet.WaitedMS
			return res, nil
		}
		return res, &HTTPError{
			Method:     http.MethodPost,
			URL:        endpoint,
			Status:     resp.StatusCode,
			StatusText: http.StatusText(resp.StatusCode),
			Body:       string(respBody),
			TraceID:    res.TraceID,
		}
	}

	fleet, ok := parseFleetOutcome(respBody)
	if !ok {
		// A 2xx we cannot read: do not invent a state. The caller gets a result
		// with an empty State, which the wrapper refuses to report as success.
		return res, nil
	}
	res.State = fleet.State
	res.Invalidated = fleet.Invalidated
	res.Checked = fleet.Checked
	res.Scope = fleet.Scope
	res.NodesApplied = fleet.NodesApplied
	res.NodesTotal = fleet.NodesTotal
	res.Lagging = fleet.Lagging
	res.EventID = fleet.EventID
	res.WaitedMS = fleet.WaitedMS
	return res, nil
}

// queryParams renders the optional `wait` / `timeout_ms` parameters.
//
// An unset field is OMITTED rather than sent as its default, so the server
// applies its own configured default (`wait=converged`, `timeout_ms` =
// `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS`, default 2000 ms) — and so a caller can
// tell "I did not ask" from "I asked for the default". Both were validated in
// New, so no escaping is needed: the values are a fixed token and an integer.
func (c *Client) queryParams() string {
	params := url.Values{}
	if c.waitMode != "" {
		params.Set("wait", string(c.waitMode))
	}
	if c.timeoutMS != 0 {
		params.Set("timeout_ms", strconv.Itoa(c.timeoutMS))
	}
	return params.Encode()
}

// fleetOutcome is the subset of the response body this SDK relies on.
type fleetOutcome struct {
	State        FleetState
	Invalidated  int64
	Checked      int64
	Scope        string
	NodesApplied int64
	NodesTotal   int64
	Lagging      []string
	EventID      string
	WaitedMS     int64
}

// parseFleetOutcome decodes the documented invalidate response envelope.
//
// It reports ok=false when the body is not JSON, has no `fleet` object, or
// carries a state outside the documented four. Callers then treat the answer as
// un-decodable rather than guessing a state — guessing is how a response nobody
// understood becomes "done".
func parseFleetOutcome(body []byte) (fleetOutcome, bool) {
	var decoded struct {
		Invalidated *int64 `json:"invalidated"`
		Checked     *int64 `json:"checked"`
		Scope       string `json:"scope"`
		Fleet       *struct {
			State        string   `json:"state"`
			NodesTotal   *int64   `json:"nodes_total"`
			NodesApplied *int64   `json:"nodes_applied"`
			Lagging      []string `json:"lagging"`
			EventID      *string  `json:"event_id"`
			WaitedMS     *int64   `json:"waited_ms"`
		} `json:"fleet"`
	}
	if err := json.Unmarshal(body, &decoded); err != nil || decoded.Fleet == nil {
		return fleetOutcome{}, false
	}
	state := FleetState(decoded.Fleet.State)
	switch state {
	case FleetApplied, FleetSingleNode, FleetPending, FleetUnavailable:
	default:
		return fleetOutcome{}, false
	}
	out := fleetOutcome{
		State:   state,
		Scope:   decoded.Scope,
		Lagging: decoded.Fleet.Lagging,
	}
	if decoded.Invalidated != nil {
		out.Invalidated = *decoded.Invalidated
	}
	if decoded.Checked != nil {
		out.Checked = *decoded.Checked
	}
	if decoded.Fleet.NodesApplied != nil {
		out.NodesApplied = *decoded.Fleet.NodesApplied
	}
	if decoded.Fleet.NodesTotal != nil {
		out.NodesTotal = *decoded.Fleet.NodesTotal
	}
	if decoded.Fleet.EventID != nil {
		out.EventID = *decoded.Fleet.EventID
	}
	if decoded.Fleet.WaitedMS != nil {
		out.WaitedMS = *decoded.Fleet.WaitedMS
	}
	// `lagging` is a JSON array in the contract; a null decodes to nil, which
	// reads as "none". Normalise to a non-nil empty slice so callers can
	// range over it unconditionally.
	if out.Lagging == nil {
		out.Lagging = []string{}
	}
	return out, true
}

// probeLeader reports whether node is reachable and whether it currently holds
// the leader lease.
//
// `/healthz/leader` is an **admin-port** route (200 active / 503 standby /
// 404 on non-candidate nodes); the data plane does not serve it. Only a
// transport failure means the node is unreachable — any HTTP answer, including
// 404 from a data-plane port, means "alive, not the leader". Treating a
// non-200 as death would make the documented data-plane node list quarantine
// itself until the pool was empty.
func (c *Client) probeLeader(ctx context.Context, node string) (alive bool, leader bool) {
	probeCtx, cancel := context.WithTimeout(ctx, c.probeTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(probeCtx, http.MethodGet, node+"/healthz/leader", nil)
	if err != nil {
		return false, false
	}
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return false, false
	}
	defer resp.Body.Close()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
	return true, resp.StatusCode == http.StatusOK
}

func (c *Client) removeNode(node string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.active = removeString(c.active, node)
	if !contains(c.removed, node) {
		c.removed = append(c.removed, node)
	}
}

// isNodeFailure reports whether an invalidation error should cause the node to
// be quarantined. HTTP 4xx errors (other than 401/403 handled by the caller)
// are treated as node/API incompatibility errors and also cause rotation.
//
// A request timeout or caller cancellation is NOT a node failure: the node may
// be healthy but merely slow, so it must not be quarantined. These are checked
// before the net.Error check below because context.DeadlineExceeded also
// satisfies net.Error (it defines a Timeout() method) and would otherwise be
// misclassified as a network fault.
func isNodeFailure(err error) bool {
	if err == nil {
		return false
	}
	var httpErr *HTTPError
	if errors.As(err, &httpErr) {
		return httpErr.Status >= 500 || httpErr.Status == http.StatusNotFound || httpErr.Status == http.StatusMethodNotAllowed
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return false
	}
	var netErr net.Error
	if errors.As(err, &netErr) {
		return true
	}
	return true
}

func contains(list []string, value string) bool {
	for _, v := range list {
		if v == value {
			return true
		}
	}
	return false
}

func removeString(list []string, value string) []string {
	out := list[:0]
	for _, v := range list {
		if v != value {
			out = append(out, v)
		}
	}
	return out
}
