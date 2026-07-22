# 思考輸出要求

對整個 TaskBlock，在最終 JSON 之前必須先且僅輸出一組 `<why>...</why>`。

- 回應必須直接以精確的小寫無屬性 `<why>` 開始。不得在它之前輸出說明文字，也不得巢狀或重複 `<why>`。
- `<why>` 中的內容經 Unicode `trim()` 後必須非空，並且要針對每個帶 `[ID]` 的條目實際分析：
  1. 說話人、聽話人、省略的主語和可能的人稱；
  2. 人物關係、語氣、情緒和敬語；
  3. 術語含義及目標語言的自然表達；
  4. 佔位符、控制符、ATT token，以及 `single line`、`free line breaking`、`N lines, corresponding line by line`、`N items, corresponding item by item` 所規定的行結構；
  5. `[ID]`、行數、來源語言殘留和最終格式。
- 不得只寫「已檢查」或直接給出結論，必須寫出具體分析。不強制使用固定欄目標題；ATT 只驗證思考內容非空，不判斷分析是否正確。
- 使用精確的小寫無屬性 `</why>` 結束這一組。`</why>` 與 JSON 之間只允許空白，然後直接輸出 system Prompt 規定的 JSON；JSON 不得放進 `<why>`，也不得輸出第二組 `<why>...</why>`。
