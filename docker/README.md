# Running the AI engine with Docker

The AI engine is the only piece that benefits from containerisation (it just needs to reach
Ollama's HTTP API); `fm` itself needs direct syscall access to your files and runs on the host.

```bash
cp config/config.example.toml config/config.toml
# edit config/config.toml: point [ai].socket_path at a path under the fm-sockets volume,
# e.g. socket_path = "/sockets/ai.sock" for the container, and the matching host path for `fm`.

docker compose -f docker/docker-compose.yml up -d
docker compose -f docker/docker-compose.yml exec ollama ollama pull llama3
docker compose -f docker/docker-compose.yml exec ollama ollama pull llava
docker compose -f docker/docker-compose.yml exec ollama ollama pull nomic-embed-text

fm --config config/config.toml status
```

For local development without Docker, running Ollama natively and `python -m aiengine`
directly (see the top-level README) is simpler — the compose file exists for a reproducible,
host-independent deployment (e.g. a NAS or a server box).
