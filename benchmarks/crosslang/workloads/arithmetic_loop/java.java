class Main {
    public static void main(String[] args) {
        long sum = 0;
        long i = 0;
        while (i < 9_000_000L) {
            sum = sum + i * 3L - i / 7L;
            i++;
        }
        System.out.println(sum);
    }
}
