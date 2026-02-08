#!/usr/bin/env python3
"""Simple TCP echo server with logging for testing punchgate tunnels."""

import argparse
import asyncio
import logging
import sys

logger = logging.getLogger("echo_server")


async def handle_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    addr = writer.get_extra_info("peername")
    label = f"{addr[0]}:{addr[1]}"
    logger.info("connected  %s", label)
    total = 0
    try:
        while data := await reader.read(4096):
            total += len(data)
            logger.info("echo       %s  %d bytes: %r", label, len(data), data)
            writer.write(data)
            await writer.drain()
    except (ConnectionResetError, BrokenPipeError):
        pass
    finally:
        writer.close()
        logger.info("closed     %s  (%d bytes total)", label, total)


async def main() -> None:
    parser = argparse.ArgumentParser(description="TCP echo server")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=7777)
    args = parser.parse_args()

    server = await asyncio.start_server(handle_client, args.host, args.port)
    logger.info("listening on %s:%d", args.host, args.port)

    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s.%(msecs)03d %(levelname)s %(message)s",
        datefmt="%H:%M:%S",
    )
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        logger.info("shutting down")
        sys.exit(0)
