import { useEffect, useRef, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import { errorMessage, testApi, type ApiTestResponse } from './api';
import { CopyButton, Modal, Spinner } from './components';

export default function ApiTestDialog({ endpoint, onClose }: { endpoint: string; onClose: () => void }) {
  const [response, setResponse] = useState<ApiTestResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const sequence = useRef(0);
  const runTest = async () => {
    const current = ++sequence.current;
    setLoading(true); setError(''); setResponse(null);
    try {
      const result = await testApi(endpoint);
      if (sequence.current === current) setResponse(result);
    } catch (failure) {
      if (sequence.current === current) setError(errorMessage(failure));
    } finally {
      if (sequence.current === current) setLoading(false);
    }
  };
  useEffect(() => {
    void runTest();
    return () => { sequence.current += 1; };
  }, [endpoint]);

  let formatted = response?.body ?? '';
  let isJson = false;
  if (response) {
    try { formatted = JSON.stringify(JSON.parse(response.body), null, 2); isJson = true; }
    catch { /* Preserve error pages and other non-JSON responses as text. */ }
  }
  const url = response?.url ?? `${endpoint.replace(/\/+$/, '')}/models`;
  return <Modal title="Test API" onClose={onClose} wide>
    <div className="api-test-body">
      <code className="api-test-request" aria-label="API test request">GET {url}</code>
      {loading && <p className="api-test-loading" role="status"><Spinner size={15} /> Testing API…</p>}
      {error && <p className="inline-error" role="alert">{error}</p>}
      {response && <>
        <div className="api-test-result-heading">
          <div><span className={`tag ${response.status >= 200 && response.status < 300 ? 'green' : 'api-test-http-error'}`}>HTTP {response.status} {response.status_text}</span><span className="subtle">{response.elapsed_ms} ms</span><span className="subtle">{isJson ? 'JSON · formatted' : 'Text'}</span></div>
          <CopyButton value={formatted} label="Copy API response" disabled={!formatted} />
        </div>
        {!isJson && <p className="subtle tiny">{response.body ? 'The response is not JSON. Showing the original text.' : 'The response body is empty.'}</p>}
        <pre className="api-test-response" aria-label="API response" tabIndex={0}><code>{formatted}</code></pre>
      </>}
    </div>
    <footer className="modal-footer"><button onClick={onClose}>Close</button><button className="primary" aria-label="Test again" disabled={loading} onClick={() => void runTest()}>{loading ? <Spinner size={14} /> : <RefreshCw size={14} />} Test again</button></footer>
  </Modal>;
}
