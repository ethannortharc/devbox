# Docker — Container Essentials

## Containers
  docker ps                  List running containers
  docker ps -a               List all containers
  docker run -it img sh      Run interactive container
  docker exec -it name sh    Shell into running container
  docker stop name           Stop container
  docker rm name             Remove container
  docker logs -f name        Follow logs

## Images
  docker images              List images
  docker build -t name .     Build from Dockerfile
  docker pull img            Pull from registry
  docker rmi img             Remove image

## Volumes & Networks
  docker volume ls           List volumes
  docker network ls          List networks
  docker volume prune        Clean unused volumes

## Docker Compose
  docker compose up -d       Start services (detached)
  docker compose down        Stop and remove
  docker compose logs -f     Follow all logs
  docker compose ps          List services

## Cleanup
  docker system prune        Remove unused data
  docker system df           Show disk usage
