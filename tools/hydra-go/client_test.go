package hydra

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

type testNode struct {
	mu       sync.Mutex
	leader   int // HTTP status for /healthz/leader
	endpoint int // HTTP status for the tenant invalidation endpoint
	calls    atomic.Int32
	path     string // request path seen by the invalidation endpoint
	rawURL   string // full request URI (path + query) seen by the invalidation endpoint
	// body, when non-empty, replaces the default success response body. It is
	// how a test produces the documented three-state envelope (`fleet.state`).
	body string
	// traceID, when non-empty, is sent as X-Hydra-Trace-Id.
	traceID string
}

// fleetBody renders the documented invalidate response (tenant API §5.2) with
// the fields a caller actually branches on.
func fleetBody(state string, nodesApplied, nodesTotal int, lagging []string) string {
	if lagging == nil {
		lagging = []string{}
	}
	quoted := make([]string, 0, len(lagging))
	for _, l := range lagging {
		quoted = append(quoted, `"`+l+`"`)
	}
	return `{"invalidated":1,"checked":0,"tenant_id":"t-acme","scope":"tenant",` +
		`"fleet":{"state":"` + state + `","nodes_total":` + itoa(nodesTotal) +
		`,"nodes_applied":` + itoa(nodesApplied) + `,"lagging":[` + strings.Join(quoted, ",") + `],` +
		`"event_id":"1737-0","waited_ms":41}}`
}

func itoa(n int) string { return strconv.Itoa(n) }

func newTestNode(leaderStatus, endpointStatus int) *testNode {
	return &testNode{leader: leaderStatus, endpoint: endpointStatus}
}

func (n *testNode) setEndpoint(status int) {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.endpoint = status
}

func (n *testNode) setBody(body string) {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.body = body
}

func (n *testNode) handler(t *testing.T) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/healthz/leader":
			n.mu.Lock()
			status := n.leader
			n.mu.Unlock()
			if status == http.StatusOK {
				w.Header().Set("Content-Type", "application/json")
				w.WriteHeader(status)
				_, _ = w.Write([]byte(`{"leader":true}`))
				return
			}
			w.WriteHeader(status)
			return
		case r.URL.Path == "/tenant/t-acme/api/v1/auth/cache/invalidate" && r.Method == http.MethodPost:
			n.calls.Add(1)
			n.mu.Lock()
			n.path = r.URL.Path
			n.rawURL = r.URL.RequestURI()
			n.mu.Unlock()
			if auth := r.Header.Get("Authorization"); auth != "Bearer sk-tenant-token" {
				t.Errorf("unexpected Authorization header: %q", auth)
			}
			n.mu.Lock()
			status := n.endpoint
			custom := n.body
			traceID := n.traceID
			n.mu.Unlock()
			if traceID != "" {
				w.Header().Set("X-Hydra-Trace-Id", traceID)
			}
			// A 200 always carries a fleet state now: the contract says a 2xx
			// alone does not mean "done", so a body without one is not a
			// response this SDK may read as success.
			body := custom
			if body == "" {
				switch {
				case status == http.StatusOK:
					body = fleetBody("applied", 3, 3, nil)
				case status == http.StatusServiceUnavailable:
					body = fleetBody("unavailable", 0, 0, nil)
				}
			}
			if body != "" {
				w.Header().Set("Content-Type", "application/json")
			}
			w.WriteHeader(status)
			if body != "" {
				_, _ = w.Write([]byte(body))
			}
			return
		default:
			http.NotFound(w, r)
		}
	})
}

// queryOf returns the raw query string the endpoint saw.
func (n *testNode) queryOf() string {
	n.mu.Lock()
	defer n.mu.Unlock()
	raw := n.rawURL
	if i := strings.IndexByte(raw, '?'); i >= 0 {
		return raw[i+1:]
	}
	return ""
}

func newClient(t *testing.T, nodes []string) *Client {
	t.Helper()
	c, err := New(Config{
		Token:                    "sk-tenant-token",
		Nodes:                    nodes,
		DisableBackgroundRecheck: true,
		ProbeTimeout:             time.Second,
		RequestTimeout:           time.Second,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

func TestInvalidateUsesLeaderNotFirstNode(t *testing.T) {
	standby := newTestNode(http.StatusServiceUnavailable, http.StatusOK)
	leader := newTestNode(http.StatusOK, http.StatusOK)

	standbySrv := httptest.NewServer(standby.handler(t))
	defer standbySrv.Close()
	leaderSrv := httptest.NewServer(leader.handler(t))
	defer leaderSrv.Close()

	c := newClient(t, []string{standbySrv.URL, leaderSrv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("InvalidateTenantAuthCache: %v", err)
	}
	if leader.calls.Load() != 1 {
		t.Fatalf("leader endpoint calls = %d, want 1", leader.calls.Load())
	}
	if standby.calls.Load() != 0 {
		t.Fatalf("standby endpoint calls = %d, want 0", standby.calls.Load())
	}
}

func TestInvalidateFailsOverAndRemovesDeadNode(t *testing.T) {
	dead := newTestNode(http.StatusOK, http.StatusInternalServerError)
	healthy := newTestNode(http.StatusServiceUnavailable, http.StatusOK)

	deadSrv := httptest.NewServer(dead.handler(t))
	defer deadSrv.Close()
	healthySrv := httptest.NewServer(healthy.handler(t))
	defer healthySrv.Close()

	c := newClient(t, []string{deadSrv.URL, healthySrv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("InvalidateTenantAuthCache: %v", err)
	}
	if dead.calls.Load() != 1 {
		t.Fatalf("dead node endpoint calls = %d, want 1", dead.calls.Load())
	}
	if healthy.calls.Load() != 1 {
		t.Fatalf("healthy node endpoint calls = %d, want 1", healthy.calls.Load())
	}
	active := c.Nodes()
	if len(active) != 1 || active[0] != healthySrv.URL {
		t.Fatalf("active nodes = %v, want [%s]", active, healthySrv.URL)
	}
	removed := c.RemovedNodes()
	if len(removed) != 1 || removed[0] != deadSrv.URL {
		t.Fatalf("removed nodes = %v, want [%s]", removed, deadSrv.URL)
	}
}

func TestProbeRemovedNodesRestoresReachableNode(t *testing.T) {
	node := newTestNode(http.StatusOK, http.StatusInternalServerError)
	healthy := newTestNode(http.StatusServiceUnavailable, http.StatusOK)

	nodeSrv := httptest.NewServer(node.handler(t))
	defer nodeSrv.Close()
	healthySrv := httptest.NewServer(healthy.handler(t))
	defer healthySrv.Close()

	c := newClient(t, []string{nodeSrv.URL, healthySrv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("first invalidate: %v", err)
	}
	if len(c.Nodes()) != 1 || len(c.RemovedNodes()) != 1 {
		t.Fatalf("before restore active=%v removed=%v", c.Nodes(), c.RemovedNodes())
	}

	// The node is reachable again; the next probe should add it back.
	node.setEndpoint(http.StatusOK)
	c.ProbeRemovedNodes(context.Background())

	active := c.Nodes()
	if len(active) != 2 {
		t.Fatalf("active nodes after restore = %v, want 2", active)
	}
	if len(c.RemovedNodes()) != 0 {
		t.Fatalf("removed nodes after restore = %v, want empty", c.RemovedNodes())
	}
}

func TestSingleNodeWithoutLeaderProbe(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("InvalidateTenantAuthCache: %v", err)
	}
	if node.calls.Load() != 1 {
		t.Fatalf("endpoint calls = %d, want 1", node.calls.Load())
	}
}

// TestFallbackTriggersWhenLeaderProbeIs404 pins the single-node / data-plane
// fallback: `/healthz/leader` is an ADMIN-port route, so a node list pointing
// at data-plane ports (the documented configuration) gets 404 from the probe
// while being perfectly able to serve the invalidation. The client must treat
// that as "alive, not the leader" and send the invalidation directly instead of
// erroring out.
func TestFallbackTriggersWhenLeaderProbeIs404(t *testing.T) {
	// A 404 probe is what every data-plane node returns; two of them exercises
	// the multi-node case, where the leader-candidate list stays empty.
	first := newTestNode(http.StatusNotFound, http.StatusOK)
	second := newTestNode(http.StatusNotFound, http.StatusOK)

	firstSrv := httptest.NewServer(first.handler(t))
	defer firstSrv.Close()
	secondSrv := httptest.NewServer(second.handler(t))
	defer secondSrv.Close()

	c := newClient(t, []string{firstSrv.URL, secondSrv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("InvalidateTenantAuthCache must fall back to a direct send, got: %v", err)
	}
	if first.calls.Load() != 1 {
		t.Fatalf("first node endpoint calls = %d, want 1", first.calls.Load())
	}
	if second.calls.Load() != 0 {
		t.Fatalf("second node endpoint calls = %d, want 0", second.calls.Load())
	}
	// A 404 from the probe is not evidence of a dead node: nothing may be
	// quarantined, or a data-plane node list would empty itself out.
	if len(c.RemovedNodes()) != 0 {
		t.Fatalf("removed nodes = %v, want empty", c.RemovedNodes())
	}
	if len(c.Nodes()) != 2 {
		t.Fatalf("active nodes = %v, want both", c.Nodes())
	}
}

// TestInvalidateTargetsTenantDataPlanePath is the regression assertion for the
// 2026-09-17 route move: the invalidation endpoint lives under the data-plane
// reserved prefix `/tenant/{id}/api/v1/...`, NOT on the management API at
// `/api/v1/tenants/{id}/...` (which no longer exists, and which is served on
// the admin port, not the data-plane port this SDK is pointed at).
func TestInvalidateTargetsTenantDataPlanePath(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("InvalidateTenantAuthCache: %v", err)
	}

	node.mu.Lock()
	gotPath := node.path
	gotURL := node.rawURL
	node.mu.Unlock()

	if gotPath != "/tenant/t-acme/api/v1/auth/cache/invalidate" {
		t.Fatalf("request path = %q, want %q", gotPath, "/tenant/t-acme/api/v1/auth/cache/invalidate")
	}
	for _, forbidden := range []string{"/api/v1/tenants/", "tenants/t-acme"} {
		if strings.Contains(gotURL, forbidden) {
			t.Fatalf("request URI %q must not target the removed management route (contains %q)", gotURL, forbidden)
		}
	}
}

// TestTenantIDIsPathEscaped keeps the URL encoding of the tenant id intact: an
// unescaped separator would let a hostile/hand-typed id rewrite the route.
func TestTenantIDIsPathEscaped(t *testing.T) {
	var gotPath string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath = r.URL.EscapedPath()
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(fleetBody("single_node", 1, 1, nil)))
	}))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t/acme"); err != nil {
		t.Fatalf("InvalidateTenantAuthCache: %v", err)
	}
	if gotPath != "/tenant/t%2Facme/api/v1/auth/cache/invalidate" {
		t.Fatalf("escaped path = %q, want %q", gotPath, "/tenant/t%2Facme/api/v1/auth/cache/invalidate")
	}
}

func TestAuthErrorDoesNotRemoveNode(t *testing.T) {
	node := newTestNode(http.StatusOK, http.StatusUnauthorized)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	err := c.InvalidateTenantAuthCache(context.Background(), "t-acme")
	if err == nil {
		t.Fatal("expected auth error")
	}
	if !strings.Contains(err.Error(), "401") {
		t.Fatalf("error = %v, want 401", err)
	}
	if len(c.RemovedNodes()) != 0 {
		t.Fatalf("removed nodes = %v, want empty", c.RemovedNodes())
	}
	if len(c.Nodes()) != 1 {
		t.Fatalf("active nodes = %v, want 1", c.Nodes())
	}
}

func TestInvalidateTenantAuthCacheKeysSendsBody(t *testing.T) {
	var gotBody string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/healthz/leader":
			w.WriteHeader(http.StatusOK)
		case r.URL.Path == "/tenant/t-acme/api/v1/auth/cache/invalidate" && r.Method == http.MethodPost:
			buf := make([]byte, 4096)
			n, _ := r.Body.Read(buf)
			gotBody = string(buf[:n])
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write([]byte(fleetBody("applied", 1, 1, nil)))
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	if err := c.InvalidateTenantAuthCacheKeys(context.Background(), "t-acme", []string{"key1", "key2"}); err != nil {
		t.Fatalf("InvalidateTenantAuthCacheKeys: %v", err)
	}
	if !strings.Contains(gotBody, `"key1"`) || !strings.Contains(gotBody, `"key2"`) {
		t.Fatalf("body = %q, want api_keys with key1/key2", gotBody)
	}
}

func TestBackgroundRecheckRestoresRemovedNode(t *testing.T) {
	node := newTestNode(http.StatusOK, http.StatusInternalServerError)
	healthy := newTestNode(http.StatusServiceUnavailable, http.StatusOK)

	nodeSrv := httptest.NewServer(node.handler(t))
	defer nodeSrv.Close()
	healthySrv := httptest.NewServer(healthy.handler(t))
	defer healthySrv.Close()

	c, err := New(Config{
		Token:           "sk-tenant-token",
		Nodes:           []string{nodeSrv.URL, healthySrv.URL},
		ProbeTimeout:    time.Second,
		RequestTimeout:  time.Second,
		RecheckInterval: 10 * time.Millisecond,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer c.Close()

	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("invalidate: %v", err)
	}
	if len(c.Nodes()) != 1 || len(c.RemovedNodes()) != 1 {
		t.Fatalf("before recovery active=%v removed=%v", c.Nodes(), c.RemovedNodes())
	}

	node.setEndpoint(http.StatusOK)
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if len(c.Nodes()) == 2 && len(c.RemovedNodes()) == 0 {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("node was not restored: active=%v removed=%v", c.Nodes(), c.RemovedNodes())
}

// TestIsNodeFailureTimeoutSemantics anchors the cross-SDK decision that a
// timeout is not a node death. This test documents the reference semantics that
// the TS and Python SDKs must align with:
//   - context.DeadlineExceeded / context.Canceled -> not a node failure (do not
//     quarantine)
//   - a real network error (connection refused) -> node failure (quarantine)
func TestIsNodeFailureTimeoutSemantics(t *testing.T) {
	if isNodeFailure(context.DeadlineExceeded) {
		t.Fatal("context.DeadlineExceeded should not be a node failure")
	}
	if isNodeFailure(context.Canceled) {
		t.Fatal("context.Canceled should not be a node failure")
	}
	// A real network error (e.g. connection refused) must quarantine the node.
	refused := &net.OpError{
		Op:  "dial",
		Net: "tcp",
		Err: errors.New("connection refused"),
	}
	if !isNodeFailure(refused) {
		t.Fatal("net.Error (connection refused) should be a node failure")
	}
}

// ---------------------------------------------------------------------------
// Three-state contract (tenant API §5.2)
// ---------------------------------------------------------------------------

// TestAppliedIsSuccessAndCarriesFleetFields pins the "done" case: HTTP 200 with
// fleet.state=applied is complete success, and the documented per-field response
// body is surfaced rather than discarded.
func TestAppliedIsSuccessAndCarriesFleetFields(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	node.setBody(fleetBody("applied", 3, 3, nil))
	node.traceID = "hydra-18d5fdcf0b0c9154-13"
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	res, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil)
	if err != nil {
		t.Fatalf("InvalidateTenantAuthCacheResult: %v", err)
	}
	if res.HTTPStatus != http.StatusOK {
		t.Fatalf("HTTPStatus = %d, want 200", res.HTTPStatus)
	}
	if res.State != FleetApplied {
		t.Fatalf("State = %q, want %q", res.State, FleetApplied)
	}
	if !res.State.Done() {
		t.Fatal("applied must be a done state")
	}
	if res.NodesApplied != 3 || res.NodesTotal != 3 {
		t.Fatalf("nodes = %d/%d, want 3/3", res.NodesApplied, res.NodesTotal)
	}
	if res.EventID != "1737-0" {
		t.Fatalf("EventID = %q, want 1737-0", res.EventID)
	}
	if res.WaitedMS != 41 {
		t.Fatalf("WaitedMS = %d, want 41", res.WaitedMS)
	}
	if res.TraceID != "hydra-18d5fdcf0b0c9154-13" {
		t.Fatalf("TraceID = %q, want the X-Hydra-Trace-Id header", res.TraceID)
	}
	if res.Scope != "tenant" {
		t.Fatalf("Scope = %q, want tenant", res.Scope)
	}
	if res.Node != srv.URL {
		t.Fatalf("Node = %q, want %q", res.Node, srv.URL)
	}
	// The wrapper agrees.
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("wrapper must succeed for applied, got %v", err)
	}
}

// TestSingleNodeIsSuccess pins the other "done" token. It is a distinct value
// from `applied` on purpose (single-node deployments never wait for peers), and
// the contract calls both complete.
func TestSingleNodeIsSuccess(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	node.setBody(fleetBody("single_node", 1, 1, nil))
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	res, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil)
	if err != nil {
		t.Fatalf("InvalidateTenantAuthCacheResult: %v", err)
	}
	if res.State != FleetSingleNode || !res.State.Done() {
		t.Fatalf("State = %q done=%v, want single_node/done", res.State, res.State.Done())
	}
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("wrapper must succeed for single_node, got %v", err)
	}
}

// TestPendingIsNotReportedAsDone is the core regression: a 202 `pending` is a
// 2xx, and the old SDKs reported any 2xx as complete success. It must now be
// distinguishable, retryable, and must still expose the fleet fields.
func TestPendingIsNotReportedAsDone(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusAccepted)
	node.setBody(fleetBody("pending", 1, 3, []string{"hydra-2", "hydra-3"}))
	node.traceID = "hydra-trace-pending"
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})

	res, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil)
	if err != nil {
		t.Fatalf("the result method must report pending as a result, not an error: %v", err)
	}
	if res.HTTPStatus != http.StatusAccepted {
		t.Fatalf("HTTPStatus = %d, want 202", res.HTTPStatus)
	}
	if res.State != FleetPending {
		t.Fatalf("State = %q, want pending", res.State)
	}
	if res.State.Done() {
		t.Fatal("pending must NOT be a done state")
	}
	if !res.State.Retryable() {
		t.Fatal("pending must be retryable")
	}
	// The fleet fields must be visible so the caller can decide what to do.
	if res.NodesApplied != 1 || res.NodesTotal != 3 {
		t.Fatalf("nodes = %d/%d, want 1/3", res.NodesApplied, res.NodesTotal)
	}
	if len(res.Lagging) != 2 || res.Lagging[0] != "hydra-2" || res.Lagging[1] != "hydra-3" {
		t.Fatalf("Lagging = %v, want [hydra-2 hydra-3]", res.Lagging)
	}
	if res.EventID != "1737-0" {
		t.Fatalf("EventID = %q, want 1737-0", res.EventID)
	}
	if res.WaitedMS != 41 {
		t.Fatalf("WaitedMS = %d, want 41", res.WaitedMS)
	}
	if res.TraceID != "hydra-trace-pending" {
		t.Fatalf("TraceID = %q, want hydra-trace-pending", res.TraceID)
	}

	// The wrapper must FAIL for pending: this is the documented behaviour change.
	err = c.InvalidateTenantAuthCache(context.Background(), "t-acme")
	if err == nil {
		t.Fatal("wrapper must not report 202 pending as success")
	}
	var pendingErr *InvalidatePendingError
	if !errors.As(err, &pendingErr) {
		t.Fatalf("error = %v (%T), want *InvalidatePendingError", err, err)
	}
	if pendingErr.Result.State != FleetPending {
		t.Fatalf("pendingErr.Result.State = %q, want pending", pendingErr.Result.State)
	}
	if !strings.Contains(err.Error(), "pending") {
		t.Fatalf("error text should name the state: %v", err)
	}

	// Pending is NOT a node failure: the node answered correctly.
	if len(c.RemovedNodes()) != 0 {
		t.Fatalf("removed nodes = %v, want empty on 202", c.RemovedNodes())
	}
	if len(c.Nodes()) != 1 {
		t.Fatalf("active nodes = %v, want 1", c.Nodes())
	}
}

// TestUnavailableIsDistinctAndDoesNotQuarantineTheNode is the other half of the
// defect: 503 `unavailable` means "the cluster was NOT notified", which is not
// the same as "this node is dead". The node answered, so it stays in the pool
// and the caller gets a distinguishable, traceable outcome.
func TestUnavailableIsDistinctAndDoesNotQuarantineTheNode(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusServiceUnavailable)
	node.setBody(fleetBody("unavailable", 0, 0, nil))
	node.traceID = "hydra-trace-unavailable"
	// A second node exists and would answer `applied` if the client rotated to
	// it. Rotating was the defect: it replaced "the cluster was not notified"
	// with an unrelated "this node is broken".
	other := newTestNode(http.StatusNotFound, http.StatusOK)
	other.setBody(fleetBody("applied", 3, 3, nil))

	nodeSrv := httptest.NewServer(node.handler(t))
	defer nodeSrv.Close()
	otherSrv := httptest.NewServer(other.handler(t))
	defer otherSrv.Close()

	c := newClient(t, []string{nodeSrv.URL, otherSrv.URL})

	res, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil)
	if err != nil {
		t.Fatalf("the result method must report unavailable as a result: %v", err)
	}
	if res.HTTPStatus != http.StatusServiceUnavailable {
		t.Fatalf("HTTPStatus = %d, want 503", res.HTTPStatus)
	}
	if res.State != FleetUnavailable {
		t.Fatalf("State = %q, want unavailable", res.State)
	}
	if res.State.Done() {
		t.Fatal("unavailable must NOT be a done state")
	}
	if !res.State.Retryable() {
		t.Fatal("unavailable must be retryable")
	}
	if res.TraceID != "hydra-trace-unavailable" {
		t.Fatalf("TraceID = %q, want the header value", res.TraceID)
	}
	if node.calls.Load() != 1 {
		t.Fatalf("first node calls = %d, want 1", node.calls.Load())
	}
	if other.calls.Load() != 0 {
		t.Fatalf("client rotated to another node on 503 (calls=%d); it must not", other.calls.Load())
	}
	if len(c.RemovedNodes()) != 0 {
		t.Fatalf("removed nodes = %v, want empty: a 503 answer is a live node", c.RemovedNodes())
	}
	if len(c.Nodes()) != 2 {
		t.Fatalf("active nodes = %v, want both still active", c.Nodes())
	}

	// The wrapper surfaces it as its own distinct error type, not a node failure.
	err = c.InvalidateTenantAuthCache(context.Background(), "t-acme")
	if err == nil {
		t.Fatal("wrapper must not report 503 unavailable as success")
	}
	var unavailableErr *InvalidateUnavailableError
	if !errors.As(err, &unavailableErr) {
		t.Fatalf("error = %v (%T), want *InvalidateUnavailableError", err, err)
	}
	if unavailableErr.Result.State != FleetUnavailable {
		t.Fatalf("state = %q, want unavailable", unavailableErr.Result.State)
	}
	// And it must be tellable apart from pending.
	var pendingErr *InvalidatePendingError
	if errors.As(err, &pendingErr) {
		t.Fatal("unavailable must not be reported as pending")
	}
	if len(c.RemovedNodes()) != 0 || len(c.Nodes()) != 2 {
		t.Fatalf("wrapper quarantined the node: active=%v removed=%v", c.Nodes(), c.RemovedNodes())
	}
}

// TestNonFleetServerErrorStillFailsOver pins that only fleet-aware 503s stop the
// rotation: an unrelated 500 with no documented envelope is still a node
// failure, so pre-existing failover behaviour is unchanged.
func TestNonFleetServerErrorStillFailsOver(t *testing.T) {
	broken := newTestNode(http.StatusNotFound, http.StatusInternalServerError)
	healthy := newTestNode(http.StatusNotFound, http.StatusOK)

	brokenSrv := httptest.NewServer(broken.handler(t))
	defer brokenSrv.Close()
	healthySrv := httptest.NewServer(healthy.handler(t))
	defer healthySrv.Close()

	c := newClient(t, []string{brokenSrv.URL, healthySrv.URL})
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err != nil {
		t.Fatalf("invalidte: %v", err)
	}
	if broken.calls.Load() != 1 || healthy.calls.Load() != 1 {
		t.Fatalf("calls broken=%d healthy=%d, want 1/1", broken.calls.Load(), healthy.calls.Load())
	}
	if len(c.RemovedNodes()) != 1 || c.RemovedNodes()[0] != brokenSrv.URL {
		t.Fatalf("removed = %v, want [%s]", c.RemovedNodes(), brokenSrv.URL)
	}
}

// ---------------------------------------------------------------------------
// wait / timeout_ms plumbing
// ---------------------------------------------------------------------------

func TestWaitAndTimeoutAreSentWhenConfigured(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c, err := New(Config{
		Token:                    "sk-tenant-token",
		Nodes:                    []string{srv.URL},
		DisableBackgroundRecheck: true,
		ProbeTimeout:             time.Second,
		RequestTimeout:           time.Second,
		WaitMode:                 WaitNone,
		TimeoutMS:                2500,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer c.Close()

	if _, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil); err != nil {
		t.Fatalf("InvalidateTenantAuthCacheResult: %v", err)
	}
	got := node.queryOf()
	for _, want := range []string{"wait=none", "timeout_ms=2500"} {
		if !strings.Contains(got, want) {
			t.Fatalf("query = %q, want it to contain %q", got, want)
		}
	}
}

// TestNoWaitParamsByDefault pins the documented default: sending nothing lets
// the server apply its own default (`wait=converged`, timeout 2000 ms) rather
// than the client inventing one.
func TestNoWaitParamsByDefault(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	if _, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil); err != nil {
		t.Fatalf("InvalidateTenantAuthCacheResult: %v", err)
	}
	if got := node.queryOf(); got != "" {
		t.Fatalf("query = %q, want empty (server default)", got)
	}
}

func TestConvergedWaitIsSentExplicitly(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c, err := New(Config{
		Token:                    "sk-tenant-token",
		Nodes:                    []string{srv.URL},
		DisableBackgroundRecheck: true,
		WaitMode:                 WaitConverged,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	defer c.Close()

	if _, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil); err != nil {
		t.Fatalf("InvalidateTenantAuthCacheResult: %v", err)
	}
	if got := node.queryOf(); got != "wait=converged" {
		t.Fatalf("query = %q, want wait=converged", got)
	}
}

// TestWaitAndTimeoutAreValidatedClientSide keeps a typo from becoming a remote
// 400: the server rejects anything outside the documented set, so New must too.
func TestWaitAndTimeoutAreValidatedClientSide(t *testing.T) {
	cases := []struct {
		name string
		cfg  Config
		want string
	}{
		{
			name: "bad wait",
			cfg:  Config{Token: "t", Nodes: []string{"http://127.0.0.1:1"}, WaitMode: "eventually"},
			want: "invalid WaitMode",
		},
		{
			name: "zero timeout is the unset default, not an error",
			cfg:  Config{Token: "t", Nodes: []string{"http://127.0.0.1:1"}, TimeoutMS: 0},
		},
		{
			name: "timeout above the server cap",
			cfg:  Config{Token: "t", Nodes: []string{"http://127.0.0.1:1"}, TimeoutMS: 60001},
			want: "invalid TimeoutMS",
		},
		{
			name: "negative timeout",
			cfg:  Config{Token: "t", Nodes: []string{"http://127.0.0.1:1"}, TimeoutMS: -1},
			want: "invalid TimeoutMS",
		},
		{
			name: "timeout at the cap",
			cfg:  Config{Token: "t", Nodes: []string{"http://127.0.0.1:1"}, TimeoutMS: MaxTimeoutMS},
		},
		{
			name: "timeout at the floor",
			cfg:  Config{Token: "t", Nodes: []string{"http://127.0.0.1:1"}, TimeoutMS: MinTimeoutMS},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := tc.cfg
			cfg.DisableBackgroundRecheck = true
			c, err := New(cfg)
			if tc.want == "" {
				if err != nil {
					t.Fatalf("New(%+v) = %v, want success", cfg, err)
				}
				_ = c.Close()
				return
			}
			if err == nil {
				_ = c.Close()
				t.Fatalf("New(%+v) succeeded, want error containing %q", cfg, tc.want)
			}
			if !strings.Contains(err.Error(), tc.want) {
				t.Fatalf("error = %v, want it to contain %q", err, tc.want)
			}
		})
	}
}

// TestTraceIDIsCapturedOnErrorPath pins §4.2 on the failure side: a tenant is
// told to quote X-Hydra-Trace-Id when reporting a problem, so it must survive
// the error path too.
func TestTraceIDIsCapturedOnErrorPath(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusUnauthorized)
	node.traceID = "hydra-trace-401"
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	_, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil)
	if err == nil {
		t.Fatal("expected an error for 401")
	}
	var httpErr *HTTPError
	if !errors.As(err, &httpErr) {
		t.Fatalf("error = %v (%T), want *HTTPError", err, err)
	}
	// The error text must carry the trace id: the result struct is only
	// available to callers of the *Result method, and the wrapper returns only
	// an error.
	if !strings.Contains(err.Error(), "hydra-trace-401") {
		t.Fatalf("error %q must carry the X-Hydra-Trace-Id so it can be quoted", err)
	}
	if httpErr.Status != http.StatusUnauthorized {
		t.Fatalf("status = %d, want 401", httpErr.Status)
	}
}

// TestUnrecognisedSuccessBodyIsNotSuccess guards the "do not guess" rule: a 2xx
// without a documented fleet state cannot be called complete.
func TestUnrecognisedSuccessBodyIsNotSuccess(t *testing.T) {
	node := newTestNode(http.StatusNotFound, http.StatusOK)
	node.setBody(`{"invalidated":1}`)
	srv := httptest.NewServer(node.handler(t))
	defer srv.Close()

	c := newClient(t, []string{srv.URL})
	res, err := c.InvalidateTenantAuthCacheResult(context.Background(), "t-acme", nil)
	if err != nil {
		t.Fatalf("result method should still return the raw outcome: %v", err)
	}
	if res.State != "" {
		t.Fatalf("State = %q, want empty for an undecodable body", res.State)
	}
	if err := c.InvalidateTenantAuthCache(context.Background(), "t-acme"); err == nil {
		t.Fatal("a 2xx with no fleet state must not be reported as success")
	}
}
