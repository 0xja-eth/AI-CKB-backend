function replacer(key, value) {
  if (typeof value === "bigint") {
    return value.toString() + "n";
  }
  return value;
}
function reviver(key, value) {
  if (typeof value === "string" && /^\d+n$/.test(value)) {
    return BigInt(value.slice(0, -1));
  }
  return value;
}
function numberReviver(key, value) {
  if (typeof value === "string" && /^\d+n$/.test(value)) {
    return Number(value.slice(0, -1));
  }
  return value;
}

export function data2Str(obj, space?: string | number) {
  return JSON.stringify(obj, replacer, space);
}
export function str2Data<T = any>(str, noBigInt = false): T {
  return JSON.parse(str, noBigInt ? numberReviver : reviver);
}
