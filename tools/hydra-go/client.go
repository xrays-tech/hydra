// Package hydra provides a small Go SDK for the Hydra tenant self-service
// auth-cache invalidation endpoint.
//
// The client accepts one or more Hydra cluster node base URLs. Before each
// invalidation it probes /healthz/leader to discover the current active
// leader. If the chosen node fails, the client automatically rotates to the
// next available node and temporarily removes the dead node from the active
// pool. A background rechecker periodically probes removed nodes and adds them
// back once they become reachable again.
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

// Config configures a Hydra cluster client.
type Config struct {
	// Token is the tenant self-service access token. It is sent as
	// "Authorization: Bearer <token>" on every invalidation request.
	Token string

	// Nodes is a list of Hydra cluster node base URLs, e.g.
	// "http://127.0.0.1:8081". At least one node is required.
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
}

// Client is a concurrency-safe Hydra tenant SDK client with automatic leader
// discovery and node failover.
type Client struct {
	token           string
	httpClient      *http.Client
	probeTimeout    time.Duration
	requestTimeout  time.Duration
	recheckInterval time.Duration

	mu      sync.RWMutex
	active  []string // reachable candidate nodes
	removed []string // temporarily quarantined nodes

	ctx    context.Context
	cancel context.CancelFunc
	wg     sync.WaitGroup
	once   sync.Once
}

// HTTPError is returned when the server responds with a non-2xx status.
type HTTPError struct {
	Method     string
	URL        string
	Status     int
	StatusText string
	Body       string
}

func (e *HTTPError) Error() string {
	body := strings.TrimSpace(e.Body)
	if len(body) > 300 {
		body = body[:300] + "..."
	}
	if body != "" {
		return fmt.Sprintf("%s %s: unexpected HTTP %d %s: %s", e.Method, e.URL, e.Status, e.StatusText, body)
	}
	return fmt.Sprintf("%s %s: unexpected HTTP %d %s", e.Method, e.URL, e.Status, e.StatusText)
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

// InvalidateTenantAuthCache invalidates the tenant's auth cache. It discovers
// the current leader, sends POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate,
// and fails over to other nodes when the chosen node is unreachable or returns
// a server error.
func (c *Client) InvalidateTenantAuthCache(ctx context.Context, tenantID string) error {
	return c.invalidate(ctx, tenantID, nil)
}

// InvalidateTenantAuthCacheKeys invalidates only the supplied api-keys for the
// tenant. When apiKeys is empty this is the same as InvalidateTenantAuthCache.
func (c *Client) InvalidateTenantAuthCacheKeys(ctx context.Context, tenantID string, apiKeys []string) error {
	if len(apiKeys) == 0 {
		return c.InvalidateTenantAuthCache(ctx, tenantID)
	}
	return c.invalidate(ctx, tenantID, map[string]any{"api_keys": apiKeys})
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

func (c *Client) invalidate(ctx context.Context, tenantID string, body map[string]any) error {
	if strings.TrimSpace(tenantID) == "" {
		return errors.New("hydra: tenantID is required")
	}

	// Work on a snapshot so the rechecker can mutate the pools concurrently.
	c.mu.RLock()
	nodes := append([]string(nil), c.active...)
	c.mu.RUnlock()

	if len(nodes) == 0 {
		return fmt.Errorf("hydra: no available nodes (removed: %s)", strings.Join(c.RemovedNodes(), ", "))
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

	// Try current leaders first, then any reachable node. A standby node will
	// forward the mutation to the active leader; a single-node deployment has
	// no /healthz/leader route and is handled by the fallback.
	attempts := append([]string(nil), leaders...)
	for _, node := range alive {
		if !contains(attempts, node) {
			attempts = append(attempts, node)
		}
	}

	if len(attempts) == 0 {
		return fmt.Errorf("hydra: no reachable nodes (removed: %s)", strings.Join(c.RemovedNodes(), ", "))
	}

	var errs []error
	for _, node := range attempts {
		err := c.doInvalidate(ctx, node, tenantID, body)
		if err == nil {
			return nil
		}
		errs = append(errs, err)
		var httpErr *HTTPError
		if errors.As(err, &httpErr) && (httpErr.Status == http.StatusUnauthorized || httpErr.Status == http.StatusForbidden) {
			// Invalid/forbidden tenant token is a client-side error; rotating
			// to another node will not fix it.
			return err
		}
		if isNodeFailure(err) {
			c.removeNode(node)
		}
	}

	return fmt.Errorf("hydra: all %d node(s) failed: %w", len(attempts), errors.Join(errs...))
}

func (c *Client) doInvalidate(ctx context.Context, node, tenantID string, body map[string]any) error {
	endpoint := node + "/api/v1/tenants/" + url.PathEscape(tenantID) + "/auth/cache/invalidate"
	var reader io.Reader
	if body != nil {
		payload, err := json.Marshal(body)
		if err != nil {
			return err
		}
		reader = bytes.NewReader(payload)
	}
	reqCtx, cancel := context.WithTimeout(ctx, c.requestTimeout)
	defer cancel()
	req, err := http.NewRequestWithContext(reqCtx, http.MethodPost, endpoint, reader)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.token)
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}

	resp, err := c.httpClient.Do(req)
	if err != nil {
		return fmt.Errorf("hydra: request to %s failed: %w", node, err)
	}
	defer resp.Body.Close()
	respBody, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return &HTTPError{
			Method:     http.MethodPost,
			URL:        endpoint,
			Status:     resp.StatusCode,
			StatusText: http.StatusText(resp.StatusCode),
			Body:       string(respBody),
		}
	}
	return nil
}

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
func isNodeFailure(err error) bool {
	if err == nil {
		return false
	}
	var httpErr *HTTPError
	if errors.As(err, &httpErr) {
		return httpErr.Status >= 500 || httpErr.Status == http.StatusNotFound || httpErr.Status == http.StatusMethodNotAllowed
	}
	var netErr net.Error
	if errors.As(err, &netErr) {
		return true
	}
	return !errors.Is(err, context.Canceled) && !errors.Is(err, context.DeadlineExceeded)
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
