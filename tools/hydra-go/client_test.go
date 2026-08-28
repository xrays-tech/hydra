package hydra

import (
	"context"
	"net/http"
	"net/http/httptest"
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
}

func newTestNode(leaderStatus, endpointStatus int) *testNode {
	return &testNode{leader: leaderStatus, endpoint: endpointStatus}
}

func (n *testNode) setEndpoint(status int) {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.endpoint = status
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
		case r.URL.Path == "/api/v1/tenants/t-acme/auth/cache/invalidate" && r.Method == http.MethodPost:
			n.calls.Add(1)
			if auth := r.Header.Get("Authorization"); auth != "Bearer sk-tenant-token" {
				t.Errorf("unexpected Authorization header: %q", auth)
			}
			n.mu.Lock()
			status := n.endpoint
			n.mu.Unlock()
			if status >= 200 && status < 300 {
				w.Header().Set("Content-Type", "application/json")
				w.WriteHeader(status)
				_, _ = w.Write([]byte(`{"invalidated":1,"tenant_id":"t-acme"}`))
				return
			}
			w.WriteHeader(status)
			return
		default:
			http.NotFound(w, r)
		}
	})
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
		case r.URL.Path == "/api/v1/tenants/t-acme/auth/cache/invalidate" && r.Method == http.MethodPost:
			buf := make([]byte, 4096)
			n, _ := r.Body.Read(buf)
			gotBody = string(buf[:n])
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write([]byte(`{"invalidated":1}`))
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
