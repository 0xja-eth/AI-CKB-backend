import { createClient, RedisClientType } from "redis";
import querystring from "querystring";
import { data2Str, str2Data } from "../utils/json";

export const client: RedisClientType<any, any> = createClient({
  url: `redis://${process.env.REDIS_HOST}:${process.env.REDIS_PORT}`,
  socket: process.env.REDIS_TLS
    ? {
        tls: process.env.REDIS_TLS.toLowerCase() === "true",
        servername: process.env.REDIS_SNI,
      }
    : undefined,
  password: process.env.REDIS_PASSWORD,
  database: Number(process.env.REDIS_DB),
});

// @ts-ignore
client.on("error", (err: Error) => {
  console.error("Redis error: ", err);
});

export const VeryShortDuration = 10; // 60s
export const ShortDuration = 60; // 60s
export const MiddleDuration = 60 * 60; // 1h
export const LongDuration = 24 * 60 * 60; // 1d
export const VeryLongDuration = 7 * 24 * 60 * 60; // 7d

export async function connect() {
  console.log("Redis connecting...");
  await client.connect();
  console.log("Redis connected", client.isReady, client.isOpen);
}
export function cache(duration = VeryShortDuration) {
  return async (req, res, next) => {
    const url = (req.originalUrl || req.url).split("?")[0];
    const queryString = querystring.stringify(req.query);

    const key = queryString ? `GET:${url}:${queryString}` : `GET:${url}`;

    try {
      // @ts-ignore
      const cachedData = await client.get(key);

      if (cachedData) {
        res.json(str2Data(cachedData));
      } else {
        const jsonOriginal = res.json;
        res.json = async (data) => {
          try {
            // @ts-ignore
            await client.setEx(key, duration, data2Str(data));
          } catch (cacheError) {
            console.error("Cache Error", cacheError);
          }
          jsonOriginal.call(res, data);
        };
        next();
      }
    } catch (err) {
      console.error("Redis Error", err);
      next();
    }
  };
}

export function cacheCall(
  name?,
  duration = VeryShortDuration,
  caseInsensitive = false,
) {
  return async <T>(fn: (...p: any) => Promise<T>, ...args): Promise<T> => {
    const keyArgs = args.filter((arg) => typeof arg !== "object");
    const key = `FUNC:${name || fn.name}:${keyArgs
      .map(
        (arg) =>
          (caseInsensitive ? arg?.toString() : arg?.toString().toLowerCase()) ||
          "undefined",
      )
      .join(":")}`;

    let res: T | any;
    try {
      // @ts-ignore
      const cachedStr = await client.get(key);

      if (cachedStr) {
        return str2Data(cachedStr);
      } else {
        res = await fn(...args);
        // @ts-ignore
        await client.setEx(key, duration, data2Str(res));
        return res;
      }
    } catch (err) {
      console.error("Redis Error", key, res, err);
      throw err;
    }
  };
}

export async function getKeys(pattern: string, count = 100) {
  let cursor = 0;
  let keys = [];
  do {
    // 使用 SCAN 命令遍历所有匹配的键
    const {cursor: newCursor, keys: newKeys} = await client.scan(cursor, {
      MATCH: pattern, COUNT: count
    });
    cursor = newCursor;
    keys = keys.concat(newKeys);
  } while (cursor !== 0);

  return keys;
}
