# Yêu cầu xuất phần suy nghĩ

Đối với toàn bộ TaskBlock, hãy xuất đúng một khối `<why>...</why>` trước JSON cuối cùng.

- Phản hồi phải bắt đầu ngay bằng thẻ `<why>` chính xác, viết thường và không có thuộc tính. Không xuất lời dẫn trước thẻ, không lồng hay lặp lại `<why>`.
- Nội dung bên trong `<why>` phải vẫn không rỗng sau Unicode `trim()` và phải thực sự phân tích từng mục có gắn `[ID]`:
  1. người nói, người nghe, chủ ngữ bị lược bỏ và ngôi có thể có;
  2. quan hệ nhân vật, giọng điệu, cảm xúc và kính ngữ;
  3. ý nghĩa thuật ngữ và cách diễn đạt tự nhiên trong ngôn ngữ đích;
  4. chỗ dành sẵn, mã điều khiển, mỗi ATT token và cấu trúc dòng do `single line`, `free line breaking`, `N lines, corresponding line by line` hoặc `N items, corresponding item by item` quy định;
  5. giá trị `[ID]`, số dòng, phần ngôn ngữ nguồn còn sót lại và định dạng cuối cùng.
- Không được chỉ viết “đã kiểm tra” hoặc đi thẳng đến kết luận; phải đưa ra phân tích cụ thể. Không bắt buộc tiêu đề mục cố định. ATT chỉ xác minh nội dung suy nghĩ không rỗng và không phán đoán phân tích có chính xác hay không.
- Kết thúc khối duy nhất bằng thẻ `</why>` chính xác, viết thường và không có thuộc tính. Giữa `</why>` và JSON chỉ được có khoảng trắng; sau đó xuất trực tiếp JSON theo system Prompt. JSON không được nằm trong `<why>` và không được có khối `<why>...</why>` thứ hai.
