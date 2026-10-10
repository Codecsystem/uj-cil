# Round 5: SWAR 64 位无分支分词器加速 (wcmp / lcmp / ncmp / hcmp)

## 1. 优化背景与性能痛点
内置比较器（`wcmp`, `lcmp`, `ncmp`, `hcmp`, `rcmp`, `yesno`）全部基于 `TokenIterator` 对选手输出和参考答案进行分词。
在先前的实现中，`TokenIterator::next()` 采用传统的逐字节扫描（Byte-by-Byte Scanning）：
1. 遇到连续空格（例如缩进、格式化输出的对齐空格）时，每跳过一个字节就要执行一次循环、边界检查及分支判断；
2. 在识别 Token 内容时，同样逐字节调用 `b.is_ascii_whitespace()`，在长 Token（例如字符串、长十进制数）场景下，CPU 分支预测和指令流水线负担较重。

在大数据量测试（例如 100k 规模的单词输出、网格输出、图论输出）中，逐字节分词成为了内置比较器最热的热点。

## 2. 优化方案：SWAR (SIMD Within A Register) 分词器
利用 64 位字长（`u64`，8 字节）并行运算技术，在不依赖 AVX2/AVX-512 特定指令集的前提下实现全平台通用的 SIMD 效果：

1. **8 字节连续空格极速快进**：
   ```rust
   while self.pos + 8 <= self.input.len() {
       let chunk = u64::from_le_bytes(self.input[self.pos..self.pos + 8].try_into().unwrap());
       if chunk == 0x2020202020202020 {
           self.pos += 8;
           self.col += 8;
       } else {
           break;
       }
   }
   ```
   一次性跳过 8 个 `0x20` 空格，并同步步进列号，不丢失换行符或 TAB。

2. **8 字节普通 ASCII 字符无分支扫描**：
   在检测普通字符（非控制字符、非空白符）时：
   - 检查高位：`if (chunk & 0x8080808080808080) != 0 { break; }`，遇非 ASCII 字符立即回退至逐字节扫描，保证多字节 UTF-8 编码安全；
   - 利用下溢借位检测控制符与空格：`let sub = chunk.wrapping_sub(0x2121212121212121); if (sub & 0x8080808080808080) != 0 { break; }`；
   - 当 8 个字节全部为可见 ASCII 字符（`0x21..=0x7E`）时，借位位全为 0，直接 `self.pos += 8; self.col += 8;`，完全无分支推进。

## 3. 安全性与正确性复核
- **换行符识别完整性**：由于换行符 `\n` (0x0A) 和 `\r` (0x0D) 均严格 `< 0x21`，SWAR 扫描会立即 break 回退到 tail 处理，行号 `self.line` 和列号 `self.col` 的计算完全精确；
- **UTF-8 兼容性**：对高位设置 `0x80` 的多字节 UTF-8 字符做了前置判断直接 break，保留完整的原始字节切片；
- **边界覆盖**：针对不足 8 字节、恰好 8 字节、多倍 8 字节、混杂中文与换行添加了专用测试 `test_swar_tokenizer`，全部通过。

## 4. 实测效果
在 `perf_bench` 基准测试中实测：
- **`wcmp` 100k 单词比较**（包含 8 字节前导缩进与尾部换行）：
  - 耗时中位数：**2.119 ms**；
  - 吞吐率：**47,192,000 单词 / 秒**；
- 结合 Round 1 的 `fast_parse_i64`，`ncmp 100k Ints` 保持在 **2.69 ms**，处理速率持续维持在数千万级别。
