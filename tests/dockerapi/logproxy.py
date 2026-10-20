#!/usr/bin/env python3
"""Forwards a unix socket to the docker socket and writes the request line of every
HTTP request that passes, so that what a tool asks of the API can be read afterwards.

    logproxy.py LISTEN_SOCKET DOCKER_SOCKET LOG_FILE
"""
import asyncio
import os
import re
import sys

REQUEST = re.compile(rb"^(GET|HEAD|POST|PUT|DELETE|PATCH|OPTIONS) (\S+) HTTP/1\.[01]$")


async def pump(reader, writer, log=None):
    pending = b""
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            if log is not None:
                # Request lines start a line of the client's stream; bodies of the
                # few requests that have one are JSON and never look like one.
                pending += data
                *lines, pending = pending.split(b"\r\n")
                for line in lines:
                    m = REQUEST.match(line)
                    if m:
                        log.write(f"{m.group(1).decode()} {m.group(2).decode()}\n")
                        log.flush()
                pending = pending[-8192:]
            writer.write(data)
            await writer.drain()
    except (ConnectionError, asyncio.CancelledError):
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def main():
    listen, upstream, path = sys.argv[1:4]
    log = open(path, "a", buffering=1)
    if os.path.exists(listen):
        os.unlink(listen)

    async def handle(client_reader, client_writer):
        server_reader, server_writer = await asyncio.open_unix_connection(upstream)
        await asyncio.gather(
            pump(client_reader, server_writer, log),
            pump(server_reader, client_writer),
        )

    server = await asyncio.start_unix_server(handle, path=listen)
    os.chmod(listen, 0o666)
    async with server:
        await server.serve_forever()


asyncio.run(main())
