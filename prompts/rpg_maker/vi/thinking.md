# Hãy nghĩ kỹ trước đã

Trước khi viết bất kỳ JSON nào, hãy suy nghĩ toàn bộ đầu vào: xuất một khối
`<why>...</why>`, rồi đến JSON.

- Bắt đầu phản hồi ngay bằng thẻ `<why>` viết thường, không thuộc tính, và bên
  trong hãy viết phân tích thật của bạn cho từng mục `[ID]`:
  1. ai đang nói, nói với ai, chủ ngữ nào bị lược bỏ và ngôi xưng nào khả dĩ;
  2. quan hệ nhân vật, giọng điệu, cảm xúc và mức độ kính ngữ;
  3. thuật ngữ nghĩa là gì và cách nói tự nhiên trong ngôn ngữ đích;
  4. chỗ dành sẵn, mã điều khiển, dấu bảo vệ, và cấu trúc dòng mà `single line`,
     `free line breaking`, `N lines, corresponding line by line` hoặc
     `N items, corresponding item by item` yêu cầu;
  5. các giá trị `[ID]`, số dòng, phần ngôn ngữ nguồn còn sót lại và định dạng cuối
     cùng.
- Hãy lập luận cụ thể để người đọc theo được; tiêu đề mục cố định không bắt buộc.
  Sau khi bỏ khoảng trắng đầu cuối, nội dung thật vẫn phải còn lại.
- Kết thúc bằng thẻ `</why>` viết thường, không thuộc tính. Sau `</why>` chỉ có
  khoảng trắng rồi đến JSON theo hình dạng bắt buộc; JSON luôn nằm ngoài `<why>`,
  và toàn bộ khối `<why>...</why>` chỉ xuất hiện đúng một lần.
